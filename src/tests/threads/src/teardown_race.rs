//! **teardown-race** (fault-relanding repro, never to land as is): a child of
//! a hundred threads faulting fresh pages in a loop is torn down while they
//! fault, two ways, many times: SIGKILL to the whole child, and Wine's way, a
//! SIGQUIT to each thread whose handler ends that thread alone (SYS_exit), the
//! last one ending the process. The parent needs every child gone within a
//! bound; a stall shows as the bound passed or the machine's watchdog.

#![expect(unsafe_code, reason = "fork, raw thread exits, mappings written by hand")]

use core::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

const THREADS: usize = 100;
const ROUNDS: usize = 12;
const PAGE: usize = 4096;
/// Live faulting threads in the child, counted by themselves.
static STARTED: AtomicU32 = AtomicU32::new(0);

extern "C" fn quit(_: libc::c_int) {
    // SAFETY: ending this thread alone, as Wine's abort_thread does.
    let _ = unsafe { libc::syscall(libc::SYS_exit, 0) };
}

fn faulter() {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    loop {
        // SAFETY: a fresh private mapping, written page by page, then unmapped.
        unsafe {
            let at = libc::mmap(
                ptr::null_mut(),
                64 * PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if at == libc::MAP_FAILED {
                continue;
            }
            for page in 0..64 {
                ptr::write_volatile(at.cast::<u8>().add(page * PAGE), page as u8);
            }
            let _ = libc::munmap(at, 64 * PAGE);
        }
    }
}

/// The child: a shared and a private region faulted by THREADS threads until
/// it is torn down. Never returns.
fn child() -> ! {
    // SAFETY: installing a handler that only exits the thread.
    unsafe {
        let mut action: libc::sigaction = core::mem::zeroed();
        action.sa_sigaction = quit as *const () as usize;
        let _ = libc::sigaction(libc::SIGQUIT, &raw const action, ptr::null_mut());
    }
    // A large resident set to tear down, as a game's.
    let big = 200 * 1024 * 1024;
    // SAFETY: a fresh private mapping, every page written once.
    unsafe {
        let at = libc::mmap(ptr::null_mut(), big, libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS, -1, 0);
        if at != libc::MAP_FAILED {
            for page in 0..big / PAGE {
                ptr::write_volatile(at.cast::<u8>().add(page * PAGE), 1);
            }
        }
    }
    for _ in 0..THREADS {
        let _ = std::thread::Builder::new().stack_size(64 * 1024).spawn(faulter);
    }
    faulter();
    // SAFETY: not reached.
    unsafe { libc::_exit(0) }
}

fn list_tids(pid: i32) -> Vec<i32> {
    std::fs::read_dir(format!("/proc/{pid}/task"))
        .map(|dir| {
            dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The step.
pub fn check_teardown_race(fail: fn(&str) -> !) {
    for round in 0..ROUNDS {
        // SAFETY: the child runs `child` alone.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            child();
        }
        if pid < 0 {
            fail("teardown-race: fork");
        }
        // Let the threads start and fault.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if round % 2 == 0 {
            // SAFETY: the child is ours.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        } else {
            // Wine's way: every thread told to end itself, the first last.
            let mut tids = list_tids(pid);
            tids.sort_unstable();
            tids.rotate_left(1);
            for tid in tids {
                // SAFETY: a thread of our child.
                let _ = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, libc::SIGQUIT) };
            }
        }
        let start = std::time::Instant::now();
        loop {
            let mut status = 0;
            // SAFETY: `status` is this frame's.
            let got = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            if got == pid {
                break;
            }
            if start.elapsed() > std::time::Duration::from_secs(20) {
                println!("threads: teardown-race round {round}: child {pid} not gone in 20 s");
                fail("teardown-race: a torn-down child did not end");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        println!(
            "threads: teardown-race round {round} ({}) gone in {} ms",
            if round % 2 == 0 { "kill" } else { "quit" },
            start.elapsed().as_millis()
        );
    }
}
