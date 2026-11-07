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
//! * **munmap-race**: see [`munmap_race`].
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
//! * **slice-request**: `sched_setattr`'s slice request ([`slice_request`]).
//! * **affinity**: `sched_setaffinity` pins, at once, another thread too, and
//!   is inherited ([`affinity`]).

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::thread;

mod armed;
mod affinity;
mod cpu_number;
mod futex_signal;
mod munmap_race;
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

/// A read through a page another thread has just unmapped faults. A window's unmap names
/// only the leaves it took down (`docs/microkernel/M5-LIFT.md` §13, ledger 6421), so one
/// left unnamed is a thread on another processor reading memory that is someone else's by
/// now: the reader keeps reading, and the child ends by the timer rather than `SIGSEGV`.
#[allow(
    unsafe_code,
    reason = "fork, mmap and a read of a page about to go are the case"
)]
fn check_unmap_stale() {
    /// The reader has read the page, in a page both ends map.
    static READING: std::sync::atomic::AtomicPtr<std::sync::atomic::AtomicU32> =
        std::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    // SAFETY: fork, and the child uses only async-signal-safe calls and its own thread.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fork for unmap-stale");
    }
    if child == 0 {
        // SAFETY: an anonymous page, read by the thread below until it is gone.
        let page = unsafe {
            libc::mmap(
                core::ptr::null_mut(),
                4096,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if page == libc::MAP_FAILED {
            // SAFETY: ends the child.
            unsafe { libc::_exit(6) };
        }
        let word = page.cast::<std::sync::atomic::AtomicU32>();
        // SAFETY: mapped just above; nothing else names it yet.
        unsafe { (*word).store(1, std::sync::atomic::Ordering::SeqCst) };
        READING.store(word, std::sync::atomic::Ordering::SeqCst);
        let seen = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let reader_seen = Arc::clone(&seen);
        let _reader = thread::spawn(move || {
            let word = READING.load(std::sync::atomic::Ordering::SeqCst);
            loop {
                // SAFETY: mapped, until it is unmapped, which is this check's fault.
                let value = unsafe { core::ptr::read_volatile(word.cast::<u32>()) };
                reader_seen.store(value, std::sync::atomic::Ordering::Relaxed);
                core::hint::spin_loop();
            }
        });
        while seen.load(std::sync::atomic::Ordering::Relaxed) == 0 {
            thread::yield_now();
        }
        // Let the reader settle on its processor with the translation in its TLB.
        thread::sleep(std::time::Duration::from_millis(50));
        // SAFETY: the page mapped above.
        if unsafe { libc::munmap(page, 4096) } != 0 {
            // SAFETY: ends the child.
            unsafe { libc::_exit(6) };
        }
        thread::sleep(std::time::Duration::from_millis(500));
        // SAFETY: ends the child: the reader outlived its page.
        unsafe { libc::_exit(7) };
    }
    let mut status = 0;
    // SAFETY: `status` is a valid out-parameter, and `child` is this program's child.
    if unsafe { libc::waitpid(child, &raw mut status, 0) } != child {
        fail("waitpid for unmap-stale");
    }
    if !libc::WIFSIGNALED(status) || libc::WTERMSIG(status) != libc::SIGSEGV {
        fail("unmap-stale: a thread kept reading a page unmapped under it");
    }
    println!("threads: unmap-stale ok");
}

/// A file of `pages` pages holding its page number plus one at each page's start, and
/// `tail` bytes more: the descriptor.
#[allow(unsafe_code, reason = "the calls are libc's")]
fn pattern_file(pages: usize, tail: usize) -> i32 {
    // SAFETY: a name the kernel copies.
    let fd = unsafe { libc::memfd_create(c"fault-ahead".as_ptr(), 0) };
    if fd < 0 {
        fail("fault-ahead: memfd_create");
    }
    // SAFETY: a descriptor just made.
    if unsafe { libc::ftruncate(fd, (pages * PAGE + tail) as libc::off_t) } != 0 {
        fail("fault-ahead: ftruncate");
    }
    for page in 0..pages {
        let word = (page as u32 + 1).to_le_bytes();
        // SAFETY: four bytes on this stack, in range of the file just sized.
        let wrote =
            unsafe { libc::pwrite(fd, word.as_ptr().cast(), 4, (page * PAGE) as libc::off_t) };
        if wrote != 4 {
            fail("fault-ahead: pwrite");
        }
    }
    fd
}

/// `fd` mapped at any address.
#[allow(unsafe_code, reason = "the call is libc's")]
fn map_file(fd: i32, pages: usize, protection: i32) -> *mut u8 {
    // SAFETY: a fresh address, and a descriptor this program holds.
    let at = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            pages * PAGE,
            protection,
            libc::MAP_PRIVATE,
            fd,
            0,
        )
    };
    if at == libc::MAP_FAILED {
        fail("fault-ahead: mmap");
    }
    at.cast()
}

/// A word at a page of a mapping.
#[allow(unsafe_code, reason = "a read of memory the caller mapped")]
fn word_at(mapping: *mut u8, page: usize) -> u32 {
    // SAFETY: the caller's mapping, or the fault its case is about.
    unsafe { core::ptr::read_volatile(mapping.add(page * PAGE).cast::<u32>()) }
}

/// Run `case` in a child and answer the signal that ended it, 0 for none.
#[allow(unsafe_code, reason = "fork and waitpid are libc's")]
fn ends_by(case: impl FnOnce()) -> i32 {
    // SAFETY: the child runs `case` and leaves by `_exit`.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("fault-ahead: fork");
    }
    if child == 0 {
        case();
        // SAFETY: ends the child.
        unsafe { libc::_exit(0) };
    }
    let mut status = 0;
    // SAFETY: as in `check_unmap_stale`.
    if unsafe { libc::waitpid(child, &raw mut status, 0) } != child {
        fail("fault-ahead: waitpid");
    }
    if libc::WIFSIGNALED(status) {
        libc::WTERMSIG(status)
    } else {
        0
    }
}

/// What a fault brings with it (`fault_ahead`, ledger 6537) changes nothing a program can
/// see but the time: every page reads right, and the pages it may not reach still fault.
#[allow(unsafe_code, reason = "mappings, and writes through them")]
fn check_fault_ahead() {
    // A file's neighbours read right, from one in the middle out.
    let fd = pattern_file(32, 0);
    let mapping = map_file(fd, 32, libc::PROT_READ);
    if word_at(mapping, 5) != 6 {
        fail("fault-ahead: a file page read wrong");
    }
    for page in 0..32 {
        if word_at(mapping, page) != page as u32 + 1 {
            fail("fault-ahead: a neighbour read wrong");
        }
    }

    // A page wholly past the file's end is still SIGBUS, its neighbour resident.
    let short = pattern_file(1, 100);
    let mapping = map_file(short, 8, libc::PROT_READ);
    let ended = ends_by(|| {
        let _ = word_at(mapping, 0);
        let _ = word_at(mapping, 3);
    });
    if ended != libc::SIGBUS {
        fail("fault-ahead: a page past the end did not stop the program");
    }

    // A neighbour the program may not read is still SIGSEGV.
    let mapping = map_file(fd, 16, libc::PROT_READ);
    // SAFETY: a page of the mapping above.
    if unsafe { libc::mprotect(mapping.add(5 * PAGE).cast(), PAGE, libc::PROT_NONE) } != 0 {
        fail("fault-ahead: mprotect");
    }
    let ended = ends_by(|| {
        let _ = word_at(mapping, 4);
        let _ = word_at(mapping, 5);
    });
    if ended != libc::SIGSEGV {
        fail("fault-ahead: a neighbour with no access did not stop the program");
    }

    // A private page the program wrote is not put back by a neighbour's fault.
    let mapping = map_file(fd, 16, libc::PROT_READ | libc::PROT_WRITE);
    // SAFETY: a page of the private, writable mapping above.
    unsafe { core::ptr::write_volatile(mapping.add(4 * PAGE).cast::<u32>(), 0xabcd) };
    if word_at(mapping, 3) != 4 || word_at(mapping, 4) != 0xabcd {
        fail("fault-ahead: a neighbour's fault put back a page the program wrote");
    }

    // The same with the page's own translation gone, so that it is the neighbour's fault
    // that looks: the private copy comes back, not the file's page (ledger 6597 E1).
    let mapping = map_file(fd, 16, libc::PROT_READ | libc::PROT_WRITE);
    // SAFETY: a page of the private, writable mapping above.
    unsafe {
        core::ptr::write_volatile(mapping.add(4 * PAGE).cast::<u32>(), 0xabce);
        if libc::mprotect(mapping.add(4 * PAGE).cast(), PAGE, libc::PROT_NONE) != 0
            || libc::mprotect(mapping.add(4 * PAGE).cast(), PAGE, libc::PROT_READ) != 0
        {
            fail("fault-ahead: mprotect of a written page");
        }
    }
    if word_at(mapping, 3) != 4 || word_at(mapping, 4) != 0xabce {
        fail("fault-ahead: a neighbour's fault read the file under a page the program wrote");
    }

    // A shared mapping's page past the file's end, as the private one's above.
    // SAFETY: a fresh address, and a descriptor this program holds.
    let shared = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            8 * PAGE,
            libc::PROT_READ,
            libc::MAP_SHARED,
            short,
            0,
        )
    };
    if shared == libc::MAP_FAILED {
        fail("fault-ahead: mmap of a shared file");
    }
    let shared = shared.cast::<u8>();
    let ended = ends_by(|| {
        let _ = word_at(shared, 0);
        let _ = word_at(shared, 3);
    });
    if ended != libc::SIGBUS {
        fail("fault-ahead: a shared page past the end did not stop the program");
    }

    // W+X (6597 E4): a neighbour brought for a read is neither writable nor executable by
    // being brought; each use takes its own fault, and the file's page does not change.
    let mapping = map_file(fd, 16, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC);
    if word_at(mapping, 8) != 9 || word_at(mapping, 9) != 10 {
        fail("fault-ahead: a W+X mapping read wrong");
    }
    // SAFETY: a page of the private, writable mapping above.
    unsafe { core::ptr::write_volatile(mapping.add(9 * PAGE).cast::<u32>(), 0xabcf) };
    let clean = map_file(fd, 16, libc::PROT_READ);
    if word_at(mapping, 9) != 0xabcf || word_at(clean, 9) != 10 {
        fail("fault-ahead: a write to a W+X neighbour reached the file");
    }
    // An executable page, the neighbour of one read: its fetch is its own fault. `ret`.
    #[cfg(target_arch = "x86_64")]
    {
        // SAFETY: a page of the private, writable mapping above; then a call of the one
        // instruction just written there, in a mapping that may execute.
        unsafe {
            core::ptr::write_volatile(mapping.add(10 * PAGE), 0xc3_u8);
            let code = core::mem::transmute::<*mut u8, extern "C" fn()>(mapping.add(10 * PAGE));
            let _ = word_at(mapping, 11);
            code();
        }
    }

    // A walk through fresh memory reads what it wrote, and stops at its region: the private
    // file mapped right after it is not copied, so a later write to the file shows there.
    // SAFETY: a fresh reservation, unmapped below in part by the fixed mappings.
    let walk = unsafe {
        libc::mmap(
            core::ptr::null_mut(),
            24 * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    if walk == libc::MAP_FAILED {
        fail("fault-ahead: mmap of a walk");
    }
    let walk = walk.cast::<u8>();
    // SAFETY: inside the reservation, replacing what is there.
    let after = unsafe {
        libc::mmap(
            walk.add(8 * PAGE).cast(),
            16 * PAGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_FIXED,
            fd,
            0,
        )
    };
    if after == libc::MAP_FAILED {
        fail("fault-ahead: mmap of a file after a walk");
    }
    for page in 0..8 {
        // SAFETY: the walk's own pages.
        unsafe { core::ptr::write_volatile(walk.add(page * PAGE).cast::<u32>(), page as u32) };
    }
    for page in 0..8 {
        if word_at(walk, page) != page as u32 {
            fail("fault-ahead: a walk read back wrong");
        }
    }
    let word = 0x5555_u32.to_le_bytes();
    // SAFETY: four bytes on this stack, in range of the file.
    if unsafe { libc::pwrite(fd, word.as_ptr().cast(), 4, 0) } != 4 {
        fail("fault-ahead: pwrite after a walk");
    }
    if word_at(walk, 8) != 0x5555 {
        fail("fault-ahead: a walk copied the mapping after its region");
    }
    println!("threads: fault-ahead ok");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(slice_request::EXEC_PROBE) => slice_request::probe(),
        // The slice step alone, for its comparison with Linux run as an
        // ordinary program there (ledger 5232 (5)): every other step wants
        // to be init.
        // The affinity step alone, for the same comparison with Linux.
        Some("--affinity-only") => {
            println!("threads: affinity");
            affinity::check_affinity(fail);
            println!("threads: affinity ok");
            std::process::exit(0)
        }
        Some("--slice-request-only") => {
            println!("threads: slice-request");
            slice_request::check_slice_request(fail);
            println!("threads: slice-request ok");
            std::process::exit(0)
        }
        _ => {}
    }
    if std::env::args().nth(1).as_deref() == Some(affinity::EXEC_PROBE) {
        affinity::probe();
    }
    check_unmap_stale();
    check_fault_ahead();
    armed::check_armed(fail);
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

    println!("threads: munmap-race");
    munmap_race::check_munmap_race(fail);
    println!("threads: munmap-race ok");

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

    println!("threads: affinity");
    affinity::check_affinity(fail);
    println!("threads: affinity ok");

    println!("threads: all ok");
    std::process::exit(0)
}
