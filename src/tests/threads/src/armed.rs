//! A tight loop of one call, against every change a kernel or `linuxd` may have kept its answer
//! past (`docs/microkernel/RESTRICTED-EXIT.md`; ledger 6787 W4, 6998 X6). Each loop makes no call
//! but `getppid`, since any other is a look of its own: under `restricted_window` such a loop is
//! answered with nothing read, so each of these is what would go stale. They hold on any kernel,
//! Linux's too, which is what makes them the program's rules and not the window's.

use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long a change gets to reach a looping thread: long, as a miss here is a stale answer that
/// never ends, and a loaded host's slow one is no finding.
const PATIENCE: Duration = Duration::from_secs(20);

/// What the two ends of a fork share.
#[repr(C)]
struct Shared {
    /// The loop's last answer.
    answer: AtomicI64,
    /// How many it has had.
    count: AtomicU64,
    /// A handler ran, or the parent in `reparent` may end.
    handled: AtomicU64,
    /// The looping process, where the case's own fork did not make it.
    who: AtomicI64,
}

/// A page both ends of a fork map.
#[allow(unsafe_code, reason = "a shared mapping is the case's only channel")]
fn shared(fail: fn(&str) -> !) -> &'static Shared {
    // SAFETY: a fresh anonymous mapping; zero is a valid `Shared`.
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
        fail("armed: mmap");
    }
    // SAFETY: mapped above and never unmapped: both ends use it until they end.
    unsafe { &*page.cast::<Shared>() }
}

/// `getppid`, by the system call and no cache.
#[allow(unsafe_code, reason = "the call is the case")]
fn call() -> i64 {
    // SAFETY: takes nothing and touches nothing of ours.
    i64::from(unsafe { libc::syscall(libc::SYS_getppid) } as i32)
}

/// The loop: this call and nothing else, for ever.
fn spin(into: &Shared) -> ! {
    loop {
        into.answer.store(call(), Ordering::Relaxed);
        let _ = into.count.fetch_add(1, Ordering::Relaxed);
    }
}

/// The loop with about 100 us of the program's own work between calls: an interrupt lands among
/// its instructions and not on a call, so nothing but a real kick can drop an arm (ledger 7224).
fn spin_working(into: &Shared) -> ! {
    loop {
        into.answer.store(call(), Ordering::Relaxed);
        let _ = into.count.fetch_add(1, Ordering::Relaxed);
        for _ in 0..3_000 {
            core::hint::spin_loop();
        }
    }
}

/// Wait until `what` holds, or say `why`.
fn within(what: impl Fn() -> bool, why: &str, fail: fn(&str) -> !) {
    let began = Instant::now();
    while !what() {
        if began.elapsed() > PATIENCE {
            fail(why);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The loop has run, and long enough to be armed: a thousand calls.
fn running(into: &'static Shared, fail: fn(&str) -> !) {
    let from = into.count.load(Ordering::Relaxed);
    within(
        move || into.count.load(Ordering::Relaxed) > from + 1000,
        "armed: the loop did not run",
        fail,
    );
}

/// Fork a child that runs `body`.
#[allow(unsafe_code, reason = "fork is the case")]
fn forked(body: impl FnOnce(), fail: fn(&str) -> !) -> libc::pid_t {
    // SAFETY: the child uses only async-signal-safe calls, and threads it makes itself.
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail("armed: fork");
    }
    if child == 0 {
        body();
        // SAFETY: ends the child, whose body does not come back.
        unsafe { libc::_exit(99) };
    }
    child
}

/// Wait for `child` with `options`: its status.
#[allow(unsafe_code, reason = "waitpid's out-parameter")]
fn waited(child: libc::pid_t, options: i32, what: &str, fail: fn(&str) -> !) -> i32 {
    let began = Instant::now();
    loop {
        let mut status = 0;
        // SAFETY: `status` is a valid out-parameter.
        let got = unsafe { libc::waitpid(child, &raw mut status, options | libc::WNOHANG) };
        if got == child {
            return status;
        }
        if got < 0 || began.elapsed() > PATIENCE {
            fail(what);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A signal to a process.
#[allow(unsafe_code, reason = "kill takes no pointer")]
fn send(to: libc::pid_t, signal: i32) {
    // SAFETY: no pointer; a child of ours or a grandchild the case made.
    let _ = unsafe { libc::kill(to, signal) };
}

/// The parent ends: the answer changes under a loop that never looks at anything else.
#[allow(unsafe_code, reason = "_exit")]
fn reparent(working: bool, fail: fn(&str) -> !) {
    let loops = shared(fail);
    let parent = forked(
        move || {
            let looping = forked(
                move || {
                    if working {
                        spin_working(loops)
                    }
                    spin(loops)
                },
                fail,
            );
            loops.who.store(i64::from(looping), Ordering::Relaxed);
            // Until this program has seen the loop name it: then the change is all that remains.
            while loops.handled.load(Ordering::Acquire) == 0 {
                core::hint::spin_loop();
            }
            // SAFETY: ends this process, leaving the loop an orphan.
            unsafe { libc::_exit(0) }
        },
        fail,
    );
    within(
        move || i32::try_from(loops.answer.load(Ordering::Relaxed)) == Ok(parent),
        "armed: the loop never named its parent",
        fail,
    );
    // Armed by now: a thousand more calls of nothing else.
    running(loops, fail);
    let _ = loops.handled.fetch_add(1, Ordering::Release);
    let _ = waited(parent, 0, "armed: the parent did not end", fail);
    within(
        move || i32::try_from(loops.answer.load(Ordering::Relaxed)) != Ok(parent),
        "armed: getppid kept naming a parent that had ended",
        fail,
    );
    // Its reaper is whoever the kernel says; it ends by signal, and this program, as init or
    // not, need not be the one to reap it.
    send(loops.who.load(Ordering::Relaxed) as libc::pid_t, libc::SIGKILL);
    println!(
        "threads: armed {} ok",
        if working { "reparent-working" } else { "reparent" }
    );
}

/// Stop, continue and kill reach the loop, and a handler runs in it.
#[allow(unsafe_code, reason = "sigaction with a handler of one store")]
fn stop_and_kill(fail: fn(&str) -> !) {
    /// Where the handler stores.
    static HANDLED: core::sync::atomic::AtomicPtr<Shared> =
        core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());
    /// One store, async-signal-safe.
    extern "C" fn handler(_signal: i32) {
        let at = HANDLED.load(Ordering::Relaxed);
        // SAFETY: stored before the handler was installed; mapped until the process ends.
        let _ = unsafe { &*at }.handled.fetch_add(1, Ordering::Relaxed);
    }
    let loops = shared(fail);
    HANDLED.store(core::ptr::from_ref(loops).cast_mut(), Ordering::Relaxed);
    let child = forked(
        move || {
            // SAFETY: an installed handler of one store, and the struct is zeroed but for it.
            unsafe {
                let mut action: libc::sigaction = core::mem::zeroed();
                action.sa_sigaction = handler as *const () as usize;
                let _ = libc::sigaction(libc::SIGUSR1, &raw const action, core::ptr::null_mut());
            }
            spin(loops)
        },
        fail,
    );
    running(loops, fail);
    send(child, libc::SIGUSR1);
    within(
        move || loops.handled.load(Ordering::Relaxed) != 0,
        "armed: a signal's handler did not run in the loop",
        fail,
    );
    send(child, libc::SIGSTOP);
    let status = waited(child, libc::WUNTRACED, "armed: the loop did not stop", fail);
    if !libc::WIFSTOPPED(status) {
        fail("armed: the loop ended where it should have stopped");
    }
    let held = loops.count.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_millis(20));
    if loops.count.load(Ordering::Relaxed) != held {
        fail("armed: a stopped loop kept running");
    }
    send(child, libc::SIGCONT);
    running(loops, fail);
    send(child, libc::SIGKILL);
    let status = waited(child, 0, "armed: the loop did not die", fail);
    if !libc::WIFSIGNALED(status) || libc::WTERMSIG(status) != libc::SIGKILL {
        fail("armed: the loop ended some other way than the kill");
    }
    println!("threads: armed stop-kill ok");
}

/// Another thread's `exit_group` ends a thread in the loop.
#[allow(unsafe_code, reason = "_exit")]
fn sibling_exit(fail: fn(&str) -> !) {
    let loops = shared(fail);
    let child = forked(
        move || {
            let _spinner = std::thread::spawn(move || spin(loops));
            while loops.count.load(Ordering::Relaxed) < 1000 {
                std::thread::sleep(Duration::from_millis(1));
            }
            // Long enough for this thread's processor to have left its space.
            std::thread::sleep(Duration::from_millis(5));
            // SAFETY: ends the process, the loop's thread with it.
            unsafe { libc::_exit(3) }
        },
        fail,
    );
    let status = waited(child, 0, "armed: exit_group left a looping thread running", fail);
    if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 3 {
        fail("armed: the process did not end by its exit_group");
    }
    println!("threads: armed exit-group ok");
}

/// A `TSYNC` filter from another thread reaches a thread in the loop.
#[allow(unsafe_code, reason = "a seccomp filter's install")]
fn tsync(fail: fn(&str) -> !) {
    let loops = shared(fail);
    let child = forked(
        move || {
            let _spinner = std::thread::spawn(move || spin(loops));
            while loops.count.load(Ordering::Relaxed) < 1000 {
                std::thread::sleep(Duration::from_millis(1));
            }
            std::thread::sleep(Duration::from_millis(5));
            // `getppid` answers `EPERM`; anything else as it was.
            let number = libc::SYS_getppid as u32;
            let mut program = [
                libc::sock_filter { code: 0x20, jt: 0, jf: 0, k: 0 },
                libc::sock_filter { code: 0x15, jt: 0, jf: 1, k: number },
                libc::sock_filter { code: 0x06, jt: 0, jf: 0, k: 0x0005_0001 },
                libc::sock_filter { code: 0x06, jt: 0, jf: 0, k: 0x7fff_0000 },
            ];
            let filter = libc::sock_fprog {
                len: program.len() as u16,
                filter: program.as_mut_ptr(),
            };
            // SAFETY: both are the documented forms, and `filter` outlives the call.
            let installed = unsafe {
                let _ = libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                libc::syscall(libc::SYS_seccomp, 1_u64, 1_u64, &raw const filter)
            };
            if installed != 0 {
                // SAFETY: ends the child.
                unsafe { libc::_exit(9) };
            }
            let began = Instant::now();
            while loops.answer.load(Ordering::Relaxed) != -1 {
                if began.elapsed() > PATIENCE {
                    // SAFETY: ends the child.
                    unsafe { libc::_exit(8) };
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            // SAFETY: ends the child.
            unsafe { libc::_exit(0) }
        },
        fail,
    );
    let status = waited(child, 0, "armed: the TSYNC case did not end", fail);
    if !libc::WIFEXITED(status) {
        fail("armed: the TSYNC case ended by a signal");
    }
    match libc::WEXITSTATUS(status) {
        0 => println!("threads: armed tsync ok"),
        8 => fail("armed: a TSYNC filter did not reach a thread in the loop"),
        // A kernel built with no seccomp says so on its own rows; nothing was asked here.
        _ => println!("threads: armed tsync not built"),
    }
}

/// What a cgroup holds of the loop: a move, a freeze and a `cpu.max`, where there is a cgroup
/// file system to ask. Each is what the straight answer compares, and nothing else brings the
/// loop back through the kernel's or `linuxd`'s look.
#[allow(unsafe_code, reason = "the mount of the file system the case asks")]
fn cgroup(fail: fn(&str) -> !) {
    const AT: &str = "/sys/fs/cgroup/armed";
    let _ = std::fs::create_dir_all("/sys/fs/cgroup");
    // SAFETY: literal strings, and no data: as init, nothing has mounted it yet.
    let _ = unsafe {
        libc::mount(
            c"cgroup2".as_ptr(),
            c"/sys/fs/cgroup".as_ptr(),
            c"cgroup2".as_ptr(),
            0,
            core::ptr::null(),
        )
    };
    if std::fs::create_dir(AT).is_err() {
        println!("threads: armed cgroup no cgroup file system here");
        return;
    }
    let _ = std::fs::write("/sys/fs/cgroup/cgroup.subtree_control", "+cpu");
    let loops = shared(fail);
    let child = forked(move || spin(loops), fail);
    running(loops, fail);
    let rate = || {
        let from = loops.count.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        loops.count.load(Ordering::Relaxed).saturating_sub(from)
    };
    let free = rate();
    // A hundredth of a processor, on an empty group: the move is all that can reach the loop.
    let limited = std::fs::write(format!("{AT}/cpu.max"), "1000 100000").is_ok();
    if std::fs::write(format!("{AT}/cgroup.procs"), child.to_string()).is_err() {
        fail("armed: the move into a cgroup was refused");
    }
    if limited {
        std::thread::sleep(Duration::from_millis(50));
        if rate() >= free / 2 {
            fail("armed: a move into a limited group did not reach a thread in the loop");
        }
        println!("threads: armed move ok");
        // Free again, back on its armed loop; then the limit is all that can reach it.
        let _ = std::fs::write(format!("{AT}/cpu.max"), "max 100000");
        running(loops, fail);
        std::thread::sleep(Duration::from_millis(50));
        let _ = std::fs::write(format!("{AT}/cpu.max"), "1000 100000");
        std::thread::sleep(Duration::from_millis(50));
        if rate() >= free / 2 {
            fail("armed: cpu.max did not reach a thread in the loop");
        }
        let _ = std::fs::write(format!("{AT}/cpu.max"), "max 100000");
        println!("threads: armed cpu-max ok");
    } else {
        println!("threads: armed cpu-max not built");
    }
    if std::fs::write(format!("{AT}/cgroup.freeze"), "1").is_ok() {
        std::thread::sleep(Duration::from_millis(50));
        let held = loops.count.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(50));
        if loops.count.load(Ordering::Relaxed) != held {
            fail("armed: a frozen loop kept running");
        }
        let _ = std::fs::write(format!("{AT}/cgroup.freeze"), "0");
        running(loops, fail);
        println!("threads: armed freeze ok");
    }
    send(child, libc::SIGKILL);
    let _ = waited(child, 0, "armed: the cgroup's loop did not die", fail);
    let _ = std::fs::remove_dir(AT);
}

/// The straight answer is `getppid`'s number on this table and no other's: `110` is `getppid` on
/// x86-64 and `timer_settime` on AArch64, where a bad timer's `EINVAL` must not come back a pid.
#[allow(unsafe_code, reason = "the raw call is the case")]
fn only_its_own_number(fail: fn(&str) -> !) {
    // Its parent's number kept first: there is no straight answer until a `getppid` has run.
    let _ = call();
    // SAFETY: a timer that does not exist, so the two pointers are never read.
    let answer = unsafe {
        libc::syscall(
            libc::SYS_timer_settime,
            -1 as libc::c_long,
            0 as libc::c_long,
            core::ptr::null::<u8>(),
            core::ptr::null::<u8>(),
        )
    };
    if answer != -1 {
        fail("armed: timer_settime on no timer was answered as another call");
    }
    println!("threads: armed its-own-number ok");
}

/// Every case.
pub(crate) fn check_armed(fail: fn(&str) -> !) {
    println!("threads: armed");
    only_its_own_number(fail);
    reparent(false, fail);
    reparent(true, fail);
    stop_and_kill(fail);
    sibling_exit(fail);
    tsync(fail);
    cgroup(fail);
    println!("threads: armed ok");
}
