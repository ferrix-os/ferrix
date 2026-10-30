//! Stage 7's threads exit test: a static musl Rust program that uses
//! `std::thread`, `Mutex` and `mpsc` the way `rustc` does, run as init by
//! `cargo xtask test-threads` on all three architectures.
//!
//! Every step prints a line and then `<step> ok`, and the program ends with
//! `threads: all ok` and status 0, or `threads: FAILED <what>` and status 1.
//! Built with the `negative-control` feature it expects one thread more under
//! `/proc/self` than it has, and must fail on exactly that step.
//!
//! The steps:
//!
//! * **spawn**: four named threads, which each wait at a barrier before they
//!   do anything, so that all five are alive for the next step;
//! * **proc**: `/proc/self/status` counts five threads, `/proc/self/task`
//!   lists five, and each one's `status` names this process in `Tgid`;
//! * **channel**: once let past the barrier, each thread sends its index;
//! * **join**: each thread's value comes back through `join`;
//! * **mutex**: four threads' thousand increments each, under one `Mutex`,
//!   are all there;
//! * **copy**: after a fork has made this process's memory copy-on-write,
//!   and while four threads keep the other processors busy in this address
//!   space, the main one writes to sixty-four pages, each of which the
//!   kernel has to make writable again and take back from every processor
//!   that may have the old translation cached. A page fault that waits for
//!   other processors with interrupts masked stops the machine here, which
//!   is how a compositor drawing on several threads found it.
//! * **setuid**: in a forked child with a second thread blocked on a
//!   channel, root's `setuid(1000)` goes through musl's broadcast to both
//!   threads, leaves both uid and euid 1000, and `setuid(0)` is then `EPERM`
//!   (see [`setuid`] for why per-process credentials make this the check).
//! * **futex-signal**: a timed `FUTEX_WAIT` a handled `SIGUSR1` interrupts
//!   ends `EINTR` at once, under `SA_RESTART` too, as on Linux
//!   ([`futex_signal`]);
//! * **futex-stop**: one stopped and continued, with no handler, resumes to
//!   its own deadline and ends `ETIMEDOUT`;
//! * **cpu-number**: on x86-64, `RDTSCP` names the processor `getcpu` names
//!   ([`cpu_number`]).

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;

mod cpu_number;
mod futex_signal;
mod setuid;
mod slice_request;

/// Threads the program spawns besides its main one.
const THREADS: u64 = 4;

/// Increments each thread makes under the mutex.
const INCREMENTS: u64 = 1000;

/// The threads `/proc/self` is expected to count: every spawned thread and the
/// main one, and one more for the negative control.
const EXPECTED_THREADS: u64 = if cfg!(feature = "negative-control") {
    THREADS + 2
} else {
    THREADS + 1
};

/// A page, as every architecture this runs on has them.
const PAGE: usize = 4096;

/// How many pages the **copy** step writes to.
const COPIED: usize = 64;

/// The **copy** step: write to memory a child was given a copy-on-write view
/// of, with the other processors busy in this address space the whole time.
///
/// The child is a program that does not exist. Starting it fails, and
/// nothing here needs it to succeed: the kernel has forked by then, and
/// every page this process had is read-only in its tables until it is
/// written to again, which is what `std::process::Command` does to any
/// program that starts another -- a compositor starting its clients, which
/// is where this was found.
fn check_copies() {
    let mut pages = vec![1_u8; COPIED * PAGE];
    let _ = std::process::Command::new("/threads-test-has-no-such-program").spawn();

    let stop = Arc::new(AtomicBool::new(false));
    let busy: Vec<_> = (0..THREADS)
        .map(|_| {
            let stop = Arc::clone(&stop);
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
        })
        .collect();
    for page in pages.chunks_mut(PAGE) {
        if let Some(first) = page.first_mut() {
            *first = 2;
        }
        thread::yield_now();
    }
    stop.store(true, Ordering::Relaxed);
    for handle in busy {
        handle
            .join()
            .unwrap_or_else(|_| fail("a busy thread panicked"));
    }
    let written = pages
        .chunks(PAGE)
        .filter(|page| page.first() == Some(&2) && page.get(1) == Some(&1))
        .count();
    if written != COPIED {
        println!("threads: {written} of {COPIED} pages kept what was written to them");
        fail("a page written to after a fork did not keep what it held and what was written");
    }
}

/// Say what failed and end with status 1.
fn fail(what: &str) -> ! {
    println!("threads: FAILED {what}");
    std::process::exit(1)
}

/// The value of the `label:` line of a `/proc` status file, trimmed.
fn field<'a>(status: &'a str, label: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(label))
        .and_then(|rest| rest.strip_prefix(':'))
        .map(str::trim)
}

/// The **proc** step: count this process's threads the three ways `/proc`
/// tells them, while all of them are alive.
fn check_proc() {
    let pid = std::process::id().to_string();
    let status = fs::read_to_string("/proc/self/status")
        .unwrap_or_else(|_| fail("/proc/self/status could not be read"));
    let counted = field(&status, "Threads").and_then(|n| n.parse::<u64>().ok());
    if counted != Some(EXPECTED_THREADS) {
        println!("threads: /proc/self/status says Threads: {counted:?}");
        fail("/proc/self/status does not count every thread");
    }

    let tasks: Vec<String> = fs::read_dir("/proc/self/task")
        .unwrap_or_else(|_| fail("/proc/self/task could not be listed"))
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if u64::try_from(tasks.len()).ok() != Some(EXPECTED_THREADS) {
        println!("threads: /proc/self/task lists {tasks:?}");
        fail("/proc/self/task does not list every thread");
    }
    if !tasks.contains(&pid) {
        fail("/proc/self/task does not list the main thread under the pid");
    }
    for tid in &tasks {
        let path = format!("/proc/self/task/{tid}/status");
        let status = fs::read_to_string(&path)
            .unwrap_or_else(|_| fail("a thread's status under /proc/self/task could not be read"));
        if field(&status, "Tgid") != Some(pid.as_str()) || field(&status, "Pid") != Some(tid) {
            println!(
                "threads: {path} gives Tgid {:?} and Pid {:?}",
                field(&status, "Tgid"),
                field(&status, "Pid")
            );
            fail("a thread's status does not name its process and itself");
        }
    }
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(slice_request::EXEC_PROBE) => slice_request::probe(),
        // The slice step alone, for its comparison with Linux run as an
        // ordinary program there (ledger 5232 (5)): every other step wants
        // to be init.
        Some("--slice-request-only") => {
            println!("threads: slice-request");
            slice_request::check_slice_request(fail);
            println!("threads: slice-request ok");
            std::process::exit(0)
        }
        _ => {}
    }
    println!("threads: spawn {THREADS}");
    let counter = Arc::new(Mutex::new(0_u64));
    let barrier = Arc::new(Barrier::new(
        usize::try_from(THREADS + 1).unwrap_or(usize::MAX),
    ));
    let (tx, rx) = mpsc::channel();
    let mut handles = Vec::new();
    for index in 0..THREADS {
        let counter = Arc::clone(&counter);
        let barrier = Arc::clone(&barrier);
        let tx = tx.clone();
        let handle = thread::Builder::new()
            .name(format!("worker-{index}"))
            .spawn(move || {
                let _ = barrier.wait();
                for _ in 0..INCREMENTS {
                    *counter.lock().unwrap_or_else(|_| fail("a poisoned mutex")) += 1;
                }
                tx.send(index)
                    .unwrap_or_else(|_| fail("a send to a live channel"));
                index * 10
            })
            .unwrap_or_else(|error| {
                println!("threads: spawn error {error}");
                fail("thread::spawn")
            });
        handles.push(handle);
    }
    drop(tx);
    println!("threads: spawn ok");

    println!("threads: proc");
    check_proc();
    println!("threads: proc ok");
    let _ = barrier.wait();

    println!("threads: channel");
    let mut received: Vec<u64> = rx.iter().collect();
    received.sort_unstable();
    if received != (0..THREADS).collect::<Vec<_>>() {
        fail("the channel did not carry one message from each thread");
    }
    println!("threads: channel ok");

    println!("threads: join");
    let joined: u64 = handles
        .into_iter()
        .map(|handle| handle.join().unwrap_or_else(|_| fail("a thread panicked")))
        .sum();
    if joined != (0..THREADS).map(|index| index * 10).sum::<u64>() {
        fail("join did not return each thread's value");
    }
    println!("threads: join ok");

    println!("threads: mutex");
    let total = *counter.lock().unwrap_or_else(|_| fail("a poisoned mutex"));
    if total != THREADS * INCREMENTS {
        fail("the mutex lost increments");
    }
    println!("threads: mutex ok");

    println!("threads: copy");
    check_copies();
    println!("threads: copy ok");

    println!("threads: setuid");
    setuid::check_setuid(fail);
    println!("threads: setuid ok");

    println!("threads: futex-signal");
    futex_signal::check_futex_signal(fail);
    println!("threads: futex-signal ok");

    println!("threads: futex-stop");
    futex_signal::check_futex_stop(fail);
    println!("threads: futex-stop ok");

    println!("threads: cpu-number");
    cpu_number::check_cpu_number(fail);
    println!("threads: cpu-number ok");

    println!("threads: slice-request");
    slice_request::check_slice_request(fail);
    println!("threads: slice-request ok");

    println!("threads: all ok");
    std::process::exit(0)
}
