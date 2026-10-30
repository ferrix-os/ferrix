//! The **affinity** step (L.sched.99-101, `docs/GAME-MODE-SCHED.md` §2.3,
//! ledger 4446 (D11)): `sched_setaffinity` pins, as on Linux 7.0.
//!
//! * pinned to each processor in turn, the thread names it in `getcpu`
//!   (and `RDTSCP` on x86-64) on the instruction after the call and 20 ms
//!   later, and `sched_getaffinity` reads the one processor back;
//! * a thread pinned to the last processor is never seen on another over a
//!   second of spinning;
//! * a mask naming no processor, and one naming only a processor the
//!   machine does not have, are `EINVAL`;
//! * another thread's mask set while it spins on another processor: its
//!   first `getcpu` made after the call returned names the new processor;
//! * a new thread and a fork child start with their creator's mask, and
//!   `execve` keeps it (run only where the program is a file, as the
//!   slice-request step's case).
//!
//! No line prints a processor or a count, so the step's lines are Linux's
//! on any processor count.
#![allow(
    unsafe_code,
    reason = "the step's calls are raw system calls and RDTSCP"
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The argument that makes this program the exec case's probe; the
/// processor it must be pinned to follows it.
pub(crate) const EXEC_PROBE: &str = "--affinity-probe";

/// `sched_setaffinity(tid, cpus)`: zero or the error number.
fn set(tid: i32, cpus: &[usize]) -> i32 {
    // SAFETY: a zeroed `cpu_set_t` is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    for &cpu in cpus {
        // SAFETY: sets a bit of a live set; `cpu` is below CPU_SETSIZE or ignored.
        unsafe { libc::CPU_SET(cpu, &mut set) };
    }
    // SAFETY: the kernel reads `size_of` bytes of a live set.
    let answer =
        unsafe { libc::sched_setaffinity(tid, size_of::<libc::cpu_set_t>(), &raw const set) };
    if answer == 0 { 0 } else { errno() }
}

/// `sched_getaffinity(tid)` as a list of processors, or `None`.
fn get(tid: i32) -> Option<Vec<usize>> {
    // SAFETY: a zeroed `cpu_set_t` is the empty set.
    let mut set: libc::cpu_set_t = unsafe { core::mem::zeroed() };
    // SAFETY: the kernel writes at most `size_of` bytes into a live set.
    let answer =
        unsafe { libc::sched_getaffinity(tid, size_of::<libc::cpu_set_t>(), &raw mut set) };
    (answer == 0).then(|| {
        (0..libc::CPU_SETSIZE as usize)
            // SAFETY: reads a bit of a live set below CPU_SETSIZE.
            .filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &set) })
            .collect()
    })
}

/// The processor `getcpu` names.
fn getcpu() -> usize {
    let mut cpu = 0_u32;
    // SAFETY: getcpu writes one u32 through a live pointer.
    let _ = unsafe {
        libc::syscall(
            libc::SYS_getcpu,
            &raw mut cpu,
            core::ptr::null_mut::<u32>(),
            core::ptr::null_mut::<u8>(),
        )
    };
    cpu as usize
}

/// The processor `RDTSCP` names, where there is one.
fn rdtscp() -> Option<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        if core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 27) == 0 {
            return None;
        }
        let mut aux = 0_u32;
        // SAFETY: RDTSCP reads the time stamp counter and TSC_AUX.
        let _ = unsafe { core::arch::x86_64::__rdtscp(&raw mut aux) };
        Some((aux & 0xfff) as usize)
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        None
    }
}

/// The calling thread's id.
fn gettid() -> i32 {
    // SAFETY: a plain system call with no arguments.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

/// The calling thread's error number.
fn errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// A fork child that runs `body` and exits with its answer; the answer.
fn in_child(body: impl FnOnce() -> i32) -> i32 {
    // SAFETY: the child calls only raw calls and `_exit`, except in the exec
    // case, where it calls `execv`.
    match unsafe { libc::fork() } {
        0 => {
            let code = body();
            // SAFETY: ends the child without running the parent's exit path.
            unsafe { libc::_exit(code) }
        }
        -1 => -1,
        child => {
            let mut status = 0;
            // SAFETY: waits for the child just made.
            let _ = unsafe { libc::waitpid(child, &raw mut status, 0) };
            if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                -1
            }
        }
    }
}

/// The exec case's probe: exit 0 if the mask survived the exec.
pub(crate) fn probe() -> ! {
    let pinned = std::env::args()
        .nth(2)
        .and_then(|cpu| cpu.parse::<usize>().ok());
    let kept = pinned.is_some_and(|cpu| get(0) == Some(vec![cpu]) && getcpu() == cpu);
    std::process::exit(if kept { 0 } else { 1 })
}

/// The step; `fail` names what went wrong and ends the program.
pub(crate) fn check_affinity(fail: fn(&str) -> !) {
    let all = get(0).unwrap_or_else(|| fail("sched_getaffinity was refused"));
    let Some(&last) = all.last() else {
        fail("sched_getaffinity named no processor");
    };
    pinned_to_each(&all, fail);
    pinned_spinner(last, fail);
    refused(&all, fail);
    another_thread(&all, fail);
    inherited(last, fail);
    if set(0, &all) != 0 || get(0).as_ref() != Some(&all) {
        fail("the whole mask could not be given back");
    }
}

/// Pinned to each processor in turn.
fn pinned_to_each(all: &[usize], fail: fn(&str) -> !) {
    for &cpu in all {
        if set(0, &[cpu]) != 0 {
            fail("sched_setaffinity refused a processor the thread may run on");
        }
        let now = getcpu();
        let aux = rdtscp();
        if now != cpu || aux.is_some_and(|aux| aux != cpu) {
            fail("a thread that pinned itself ran its next instruction elsewhere");
        }
        if get(0).as_deref() != Some(&[cpu][..]) {
            fail("a pinned thread's mask did not read back");
        }
        thread::sleep(Duration::from_millis(20));
        if getcpu() != cpu || rdtscp().is_some_and(|aux| aux != cpu) {
            fail("a pinned thread was elsewhere 20 ms later");
        }
    }
    if set(0, all) != 0 {
        fail("sched_setaffinity refused every processor");
    }
    println!("threads: affinity: pinned to each processor in turn");
}

/// A spinner pinned to `last` for a second is never seen elsewhere.
fn pinned_spinner(last: usize, fail: fn(&str) -> !) {
    let elsewhere = thread::spawn(move || {
        if set(0, &[last]) != 0 {
            return u64::MAX;
        }
        let start = Instant::now();
        let mut elsewhere = 0_u64;
        while start.elapsed() < Duration::from_secs(1) {
            if getcpu() != last {
                elsewhere += 1;
            }
        }
        elsewhere
    })
    .join()
    .unwrap_or_else(|_| fail("the pinned spinner panicked"));
    match elsewhere {
        0 => println!("threads: affinity: a pinned spinner stayed"),
        u64::MAX => fail("the spinner could not pin itself"),
        _ => fail("a pinned spinner was seen on another processor"),
    }
}

/// An empty mask and one naming only a processor the machine lacks.
fn refused(all: &[usize], fail: fn(&str) -> !) {
    if set(0, &[]) != libc::EINVAL {
        fail("an empty mask was not EINVAL");
    }
    let missing = all.last().map_or(0, |last| last + 1);
    if missing < libc::CPU_SETSIZE as usize && set(0, &[missing]) != libc::EINVAL {
        fail("a mask naming only a processor the machine lacks was not EINVAL");
    }
    if get(0).as_deref() != Some(all) {
        fail("a refused mask changed the thread's");
    }
    println!("threads: affinity: empty and absent masks refused");
}

/// Iterations of the spinner's pure user-mode spin between two samples:
/// about a millisecond, with no system call, so that nothing but the
/// setter's interrupt brings it into the kernel during it.
const SPIN: u64 = 2_000_000;

/// Another thread's mask set while it spins in user mode: the first
/// `getcpu` it began after the call returned names the new processor. A
/// setter that returned with the spinner still on its old processor would
/// see that `getcpu` answered there, before its way out moved it.
fn another_thread(all: &[usize], fail: fn(&str) -> !) {
    if all.len() < 2 {
        println!("threads: affinity: another thread: one processor, not run");
        return;
    }
    // This thread on the first processor, the spinner alone on the last,
    // left a while to settle, so that nothing but the setter's interrupt
    // takes the spinner into the kernel while it spins in user mode.
    let (Some(&home), Some(&away)) = (all.first(), all.last()) else {
        return;
    };
    if set(0, &[home]) != 0 {
        fail("sched_setaffinity refused this thread's own processor");
    }
    let stop = Arc::new(AtomicBool::new(false));
    // The spinner's tid, then each sample: its sequence number above the
    // processor's sixteen bits.
    let tid = Arc::new(AtomicU64::new(0));
    let sample = Arc::new(AtomicU64::new(0));
    // The sequence number of the sample whose getcpu is about to begin; the
    // one the main thread watches; and that one's processor, plus one.
    let begun = Arc::new(AtomicU64::new(0));
    let watch = Arc::new(AtomicU64::new(u64::MAX));
    let answer = Arc::new(AtomicU64::new(0));
    let spinner = {
        let (stop, tid, sample, begun, watch, answer) = (
            Arc::clone(&stop),
            Arc::clone(&tid),
            Arc::clone(&sample),
            Arc::clone(&begun),
            Arc::clone(&watch),
            Arc::clone(&answer),
        );
        thread::spawn(move || {
            if set(0, &[away]) != 0 {
                return;
            }
            tid.store(gettid() as u64, Ordering::Release);
            let mut sequence = 0_u64;
            while !stop.load(Ordering::Acquire) {
                let mut spun = 0_u64;
                while spun < SPIN {
                    spun = std::hint::black_box(spun + 1);
                }
                sequence += 1;
                begun.store(sequence, Ordering::SeqCst);
                let cpu = getcpu() as u64;
                if watch.load(Ordering::SeqCst) == sequence {
                    answer.store(cpu + 1, Ordering::SeqCst);
                }
                sample.store(sequence << 16 | cpu, Ordering::Release);
            }
        })
    };
    let wait_for = |at_least: u64| {
        let start = Instant::now();
        loop {
            let seen = sample.load(Ordering::Acquire);
            if seen >> 16 >= at_least {
                return Some(seen);
            }
            if start.elapsed() > Duration::from_secs(5) {
                return None;
            }
            thread::yield_now();
        }
    };
    if wait_for(1).is_none() {
        fail("the spinning thread never sampled");
    }
    thread::sleep(Duration::from_millis(50));
    let target = home;
    let spinner_tid = tid.load(Ordering::Acquire) as i32;
    if set(spinner_tid, &[target]) != 0 {
        fail("sched_setaffinity refused another thread's mask");
    }
    // The first sample begun after this load, which is after the return:
    // the one begun now may have entered the kernel before it. The spinner
    // spins for about a millisecond before it begins the next, which leaves
    // the watch ample time to be set first.
    let next = begun.load(Ordering::SeqCst) + 1;
    watch.store(next, Ordering::SeqCst);
    let after = wait_for(next + 1);
    let watched = answer.load(Ordering::SeqCst);
    let moved = watched != 0 && watched - 1 == target as u64;
    let read_back = get(spinner_tid).as_deref() == Some(&[target][..]);
    stop.store(true, Ordering::Release);
    let _ = spinner.join();
    if after.is_none() {
        fail("the spinning thread stopped sampling");
    }
    if !moved {
        fail("another thread's next getcpu after its mask was set named a processor outside it");
    }
    if !read_back {
        fail("another thread's mask did not read back");
    }
    println!("threads: affinity: another thread moved before its next getcpu");
}

/// A new thread, a fork child and an exec keep the creator's mask.
fn inherited(last: usize, fail: fn(&str) -> !) {
    if set(0, &[last]) != 0 {
        fail("sched_setaffinity refused the last processor");
    }
    let from_thread = thread::spawn(|| (get(0), getcpu()))
        .join()
        .unwrap_or_else(|_| fail("the affinity thread panicked"));
    if from_thread != (Some(vec![last]), last) {
        fail("a new thread did not start with its creator's mask");
    }
    let from_fork = in_child(|| i32::from(get(0) != Some(vec![last]) || getcpu() != last));
    if from_fork != 0 {
        fail("a fork child did not start with its creator's mask");
    }
    let path = std::env::args().next().unwrap_or_default();
    if std::path::Path::new(&path).is_file() {
        let exe = std::ffi::CString::new(path).unwrap_or_default();
        let flag = std::ffi::CString::new(EXEC_PROBE).unwrap_or_default();
        let cpu = std::ffi::CString::new(last.to_string()).unwrap_or_default();
        let exec = in_child(|| {
            let argv = [exe.as_ptr(), flag.as_ptr(), cpu.as_ptr(), core::ptr::null()];
            // SAFETY: a null-terminated argument vector of live strings.
            let _ = unsafe { libc::execv(exe.as_ptr(), argv.as_ptr()) };
            2
        });
        match exec {
            0 => println!("threads: affinity: kept across execve"),
            2 => fail("execve of this program failed"),
            _ => fail("execve did not keep the mask"),
        }
    } else {
        println!("threads: affinity: execve not run: this program came from no file");
    }
    println!("threads: affinity: a new thread and a fork child start with the mask");
}
