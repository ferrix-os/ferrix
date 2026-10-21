//! Stage 13's scoped OOM kill check, the rest of landing M1
//! (`docs/CGROUPS.md` §6 and §7.1, `object::oom`).
//!
//! A program (`arch::USER_OOM_PROGRAM`) that writes a byte to each page of
//! 8 MiB runs in `/check-m/v`, whose `memory.max` is 1 MiB. Beside it,
//! `/check-s` holds a process with 2 MiB resident -- more than the program
//! can reach, so a kill that chose the largest process anywhere would choose
//! it. The program must end by `SIGKILL`, the sibling must still be alive,
//! `memory.events` must count one OOM kill in `v` and in `/check-m` above
//! it and nothing in `/check-s`, and an epoll set watching `v`'s
//! `memory.events` for `EPOLLPRI` must have been woken by the kill, the
//! file polling `POLLPRI` until it is read again from its start.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_elf::Class;
use ferrix_linux_abi::types::EPOLLPRI;
use ferrix_vfs::{OpenFile, Whence};
use ferrix_vma::VmaFlags;

use super::events_check::{
    LISTED_LOOK_NANOS, PATIENCE_NANOS, WAITER_ANSWER, WAITER_DONE, WAITER_SET, waiter,
};
use super::{Checked, Harness};
use crate::arch;
use crate::fs::epoll::{self, Interest};
use crate::object::job::KILLED_STATUS;
use crate::object::oom;
use crate::syscall::process::{self, Process};
use crate::syscall::{exec, image};
use crate::user::space::Access;

/// The program's cgroup's `memory.max`.
const LIMIT: &[u8] = b"1M\n";

/// The sibling's resident pages: 2 MiB.
const SIBLING_PAGES: u64 = 512;
/// Where the sibling's memory is mapped.
const SIBLING_BASE: u64 = 0x4000_0000;

/// The cookie the epoll registration carries.
const COOKIE: u64 = 0x00_0E_4B_11;

/// `SIGSEGV`'s status: what a fault past `memory.max` ended a program with
/// before the scoped OOM kill.
const SEGV_STATUS: i32 = 128 + 11;

/// What `v` may still be charged once its killed program's end can be seen:
/// its page tables and kernel objects, a few pages, and nowhere near the
/// 1 MiB it filled.
const GIVEN_BACK_BELOW: u64 = 256 * 1024;

/// How long the program may take to be killed.
const RUN_NANOS: u64 = 10_000_000_000;

/// Run it. How many processes the scoped OOM kill ended: one.
///
/// # Errors
///
/// The first thing that was not as Linux has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u64> {
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+memory\n")
        .map_err(|_| "the root refused to enable memory for the OOM check")?;
    for tail in [&b"/check-m"[..], b"/check-s"] {
        harness
            .mkdir(tail)
            .map_err(|_| "mkdir of a cgroup for the OOM check failed")?;
    }
    let _ = harness
        .write(b"/check-m/cgroup.subtree_control", b"+memory\n")
        .map_err(|_| "a cgroup refused to enable memory for its children")?;
    harness
        .mkdir(b"/check-m/v")
        .map_err(|_| "mkdir of the OOM check's limited cgroup failed")?;
    harness.report.made += 3;
    let _ = harness
        .write(b"/check-m/v/memory.max", LIMIT)
        .map_err(|_| "memory.max refused a limit")?;

    let sibling = sibling(harness)?;
    let program = program(harness)?;
    let killed = check_the_kill(harness, &program, &sibling);
    process::kill(&sibling, KILLED_STATUS);
    drop((program, sibling));
    let killed = killed?;
    for tail in [&b"/check-m/v"[..], b"/check-m", b"/check-s"] {
        harness
            .rmdir(tail)
            .map_err(|_| "rmdir of an OOM check cgroup failed")?;
    }
    let _ = harness
        .write(b"/cgroup.subtree_control", b"-memory\n")
        .map_err(|_| "the root refused to disable memory after the OOM check")?;
    Ok(killed)
}

/// A process in `/check-s` with [`SIBLING_PAGES`] resident, and no task.
fn sibling(harness: &Harness) -> Checked<Arc<Process>> {
    let sibling =
        process::new_for_check().map_err(|_| "could not make a process for the OOM check")?;
    let space = sibling.space();
    let _ = space
        .map_anonymous(
            SIBLING_BASE,
            SIBLING_PAGES * PAGE_SIZE,
            VmaFlags::READ_WRITE,
        )
        .map_err(|_| "no mapping for the OOM check's sibling")?;
    for page in 0..SIBLING_PAGES {
        space
            .fault(SIBLING_BASE + page * PAGE_SIZE, Access::WRITE)
            .map_err(|_| "the OOM check's sibling could not fault its memory in")?;
    }
    let listed = alloc::format!("{}\n", sibling.pid());
    let _ = harness
        .write(b"/check-s/cgroup.procs", listed.as_bytes())
        .map_err(|_| "a move into the OOM check's sibling cgroup failed")?;
    Ok(sibling)
}

/// The program, loaded and moved into `/check-m/v`, not yet started.
fn program(harness: &Harness) -> Checked<Arc<Process>> {
    let class = if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    };
    let file = image::build_with(
        class,
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_OOM_PROGRAM,
    );
    let program = exec::load(&file, &[b"/oom"], &[], [0x6d; ferrix_ustack::RANDOM_BYTES])
        .map_err(|_| "the OOM check's program could not be loaded")?;
    let listed = alloc::format!("{}\n", program.pid());
    let _ = harness
        .write(b"/check-m/v/cgroup.procs", listed.as_bytes())
        .map_err(|_| "a move into the OOM check's limited cgroup failed")?;
    Ok(program)
}

/// Watch `v`'s `memory.events`, run the program to its end, and require
/// the kill, its counts and its wake. How many the scoped OOM kill ended.
///
/// Verifies: L.object.80, L.object.100, H.QUOTA.6
fn check_the_kill(
    harness: &Harness,
    program: &Arc<Process>,
    sibling: &Arc<Process>,
) -> Checked<u64> {
    let events = harness
        .open_read(b"/check-m/v/memory.events")
        .map_err(|_| "memory.events did not open")?;
    if counts(&Harness::read_to_end(&events).unwrap_or_default()) != Some((0, 0)) {
        return Err("a new cgroup's memory.events does not count oom 0 and oom_kill 0");
    }
    if events.poll().priority {
        return Err("a memory.events just read, with nothing changed, polls POLLPRI");
    }
    let set = epoll::create().map_err(|_| "an epoll set was refused")?;
    epoll::of(&set)
        .ok_or("an epoll set is not one")?
        .add(
            0,
            &events,
            Interest {
                events: EPOLLPRI,
                data: COOKIE,
            },
        )
        .map_err(|_| "epoll refused to watch memory.events")?;

    let kills = oom::kills();
    // The waiter sleeps on the set's queue, which the kill's wake wakes
    // through the registration's mark (`fs::epoll`'s ready list).
    let watching = epoll::of(&set).ok_or("an epoll set is not one")?;
    let ended_before = watching.queue().waits_ended_by_a_wake();
    let answer = run_watched(&watching, &set, program)?;
    let status = answer.status;
    let woken = watching
        .queue()
        .waits_ended_by_a_wake()
        .wrapping_sub(ended_before);

    if sibling.is_terminated() {
        return Err("a process in a sibling cgroup was killed by another cgroup's OOM");
    }
    match status {
        KILLED_STATUS => {}
        0 => return Err("a program past memory.max ran to its end: nothing refused or killed it"),
        SEGV_STATUS => {
            return Err(
                "a fault past memory.max ended its program with SIGSEGV, not the scoped OOM kill",
            );
        }
        1 => return Err("the OOM check's program had its mmap refused"),
        _ => return Err("the program past memory.max did not end by SIGKILL"),
    }
    if oom::kills().wrapping_sub(kills) != 1 {
        return Err("the scoped OOM kill did not end exactly one process");
    }
    // Its memory was given back before its end could be seen: once
    // `wait_for_exit` has answered, which is what `wait4` reaps by, its space
    // holds no page and `v` is charged for none of them, though this check
    // still holds the process and so its space. Otherwise a parent in `v`
    // reaping it could be refused its next allocation at the limit the
    // victim filled (test-container's `memory.stat could not be read`).
    if program
        .space()
        .resident_pages()
        .map_or(true, |pages| pages != 0)
    {
        return Err("an OOM-killed program's pages were still resident when its end could be seen");
    }
    let current = harness
        .read(b"/check-m/v/memory.current")
        .ok()
        .and_then(|text| core::str::from_utf8(&text).ok()?.trim().parse::<u64>().ok())
        .ok_or("memory.current of the OOM check's cgroup could not be read")?;
    if current >= GIVEN_BACK_BELOW {
        return Err(
            "an OOM-killed program's cgroup was still charged its memory when its end could be seen",
        );
    }
    match answer.event {
        Some((bits, COOKIE)) if bits & EPOLLPRI != 0 => {}
        _ => {
            return Err(
                "an epoll waiting on memory.events did not come back with EPOLLPRI and its \
                 cookie after the OOM kill",
            );
        }
    }
    if woken == 0 {
        return Err("an epoll on memory.events was ended by its recheck, not by the kill's wake");
    }
    if !events.poll().priority {
        return Err("memory.events did not poll POLLPRI after an OOM kill");
    }
    let read = |file: &OpenFile| {
        file.seek(0, Whence::Set)
            .ok()
            .and_then(|_| Harness::read_to_end(file).ok())
            .and_then(|text| counts(&text))
    };
    match read(&events) {
        Some((oom, 1)) if oom >= 1 => {}
        _ => return Err("memory.events did not count the OOM and one OOM kill"),
    }
    if events.poll().priority || !epoll::of(&set).is_some_and(|set| set.ready(4).is_empty()) {
        return Err("memory.events still polled POLLPRI after it was read again from its start");
    }
    let parent = harness
        .read(b"/check-m/memory.events")
        .ok()
        .and_then(|text| counts(&text));
    let beside = harness
        .read(b"/check-s/memory.events")
        .ok()
        .and_then(|text| counts(&text));
    if !matches!(parent, Some((oom, 1)) if oom >= 1) {
        return Err("the parent's memory.events did not count its child's OOM kill");
    }
    if beside != Some((0, 0)) {
        return Err("a sibling cgroup's memory.events counted another cgroup's OOM");
    }
    Ok(oom::kills().wrapping_sub(kills))
}

/// What [`run_watched`] saw.
struct Watched {
    /// The program's status.
    status: i32,
    /// The waiter's first event's bits and cookie, if it had one.
    event: Option<(u32, u64)>,
}

/// Start the waiter on `set`, then the program, and wait for both.
/// Verifies: `L.sched.35`, `L.syscall.20`
fn run_watched(
    watching: &epoll::Epoll,
    set: &Arc<OpenFile>,
    program: &Arc<Process>,
) -> Checked<Watched> {
    *WAITER_ANSWER.lock() = None;
    *WAITER_SET.lock() = Some(Arc::clone(set));
    let task = crate::sched::spawn("oom-events-waiter", waiter, 0, ferrix_sched::NICE_0_WEIGHT)?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while watching.queue().listed() == 0
        && WAITER_ANSWER.lock().is_none()
        && crate::timer::now_nanos() < deadline
    {
        crate::sched::sleep_for(LISTED_LOOK_NANOS);
    }
    let started = process::start(program);
    let status = started.ok().and_then(|_task| {
        program.wait_for_exit(crate::timer::now_nanos().saturating_add(RUN_NANOS))
    });
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = WAITER_DONE.wait_until_deadline(|| WAITER_ANSWER.lock().is_some(), deadline);
    // No answer is judged after the program's status, which says why.
    let answer = WAITER_ANSWER.lock().take();
    *WAITER_SET.lock() = None;
    crate::sched::wait_until_gone(&task, crate::sched::REAPER_PATIENCE_NANOS)?;
    let status = status.ok_or("the program past memory.max did not end, or did not start")?;
    Ok(Watched {
        status,
        event: answer.flatten(),
    })
}

/// `oom` and `oom_kill` from a `memory.events`, if it has both.
fn counts(text: &[u8]) -> Option<(u64, u64)> {
    let text = core::str::from_utf8(text).ok()?;
    let find = |key: &str| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.parse::<u64>().ok()).flatten()
        })
    };
    Some((find("oom")?, find("oom_kill")?))
}
