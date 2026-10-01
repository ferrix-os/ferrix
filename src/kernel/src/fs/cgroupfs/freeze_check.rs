//! Stage 13's `cgroup.freeze`, landing F (`docs/CGROUPS.md` §11).
//!
//! A program of three threads (`arch::USER_STOPPED_PROGRAM`: two counting on
//! words of a page, one waiting in `FUTEX_WAIT`) runs in `/check-fz`. Then:
//!
//! * `cgroup.freeze` reads `0`; a write of `1` makes `cgroup.events` say
//!   `frozen 1` once every thread has parked, waking a poller of it with
//!   `POLLPRI`, and every task is blocked and neither count moves across a
//!   window;
//! * `SIGCONT` does not thaw it, and neither count moves after it;
//! * a write of `0` says `frozen 0` at once, both counts move again, and the
//!   waiter is back in its wait having never returned from it;
//! * frozen again, a `cgroup.kill` ends the program (status 137) and the
//!   empty cgroup then says `populated 0` and `frozen 1`;
//! * a second program, frozen, is ended by `SIGKILL`;
//! * a cgroup beneath a frozen one says `frozen 1` while its own
//!   `cgroup.freeze` reads `0`, a process moved into a frozen cgroup is
//!   frozen and out of it thawed, a fork made in a frozen cgroup starts
//!   frozen, and so does a process made in one by `Process::new`, which is
//!   what `CLONE_INTO_CGROUP` makes its child with;
//! * a write other than `0` or `1` is refused as Linux refuses it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use ferrix_elf::Class;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{SIGCONT, SIGKILL};

use super::{Checked, Harness};
use crate::arch;
use crate::object::job::Job;
use crate::object::process::Host;
use crate::object::process::Process as Core;
use crate::syscall::kill;
use crate::syscall::process::{self, Process};
use crate::syscall::signal::Origin;
use crate::syscall::{exec, futex, image, uaccess};
use crate::user::space::AddressSpace;

/// Where the program keeps its counts and its waiter's word.
const PAGE: u64 = 0x6000_0000;
/// What `SIGKILL` and `cgroup.kill` end a program with.
const KILLED: i32 = 137;
/// How long a step waits for the program to get there.
const PATIENCE_NANOS: u64 = 10_000_000_000;
/// How often it looks meanwhile.
const POLL_NANOS: u64 = 1_000_000;
/// How long frozen counts must stay still.
const STILL_NANOS: u64 = 50_000_000;

/// A program loaded, moved into `/check-fz`, and started.
pub(super) struct Running {
    /// The process.
    pub(super) process: Arc<Process>,
    /// Its task, kept so that it is not reaped under the check.
    _task: Arc<crate::sched::Task>,
}

/// The ELF class this build runs.
fn class() -> Class {
    if size_of::<usize>() == 8 {
        Class::Elf64
    } else {
        Class::Elf32
    }
}

/// Load the program, move it into the cgroup at `tail`, and start it.
pub(super) fn start(harness: &Harness, tail: &[u8]) -> Checked<Running> {
    let file = image::build_with(
        class(),
        arch::ARCH.elf_machine(),
        image::Shape::Good,
        arch::USER_STOPPED_PROGRAM,
    );
    let process = exec::load(
        &file,
        &[b"/freeze"],
        &[],
        [0x5a; ferrix_ustack::RANDOM_BYTES],
    )
    .map_err(|_| "freeze check: the program could not be loaded")?;
    let mut procs = Vec::from(tail);
    procs.extend_from_slice(b"/cgroup.procs");
    let listed = alloc::format!("{}\n", process.pid());
    let _ = harness
        .write(&procs, listed.as_bytes())
        .map_err(|_| "freeze check: the program could not be moved in")?;
    let task =
        process::start(&process).map_err(|_| "freeze check: the program could not be started")?;
    Ok(Running {
        process,
        _task: task,
    })
}

/// A word of the program's page, if it has mapped it yet.
fn try_word(process: &Process, offset: u64) -> Option<u32> {
    let mut bytes = [0_u8; 4];
    uaccess::copy_from_user(process.space(), PAGE + offset, &mut bytes)
        .ok()
        .map(|()| u32::from_le_bytes(bytes))
}

/// A word of the program's page, which it has mapped.
fn word(process: &Process, offset: u64) -> Checked<u32> {
    try_word(process, offset).ok_or("freeze check: the program's page could not be read")
}

/// Wait until `ready`, or fail with `failure`.
fn until(
    process: &Process,
    ready: &dyn Fn() -> Checked<bool>,
    failure: &'static str,
) -> Checked<()> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        if ready()? {
            return Ok(());
        }
        if process.is_terminated() {
            return Err("freeze check: the program ended before the check was done with it");
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(failure);
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
}

/// Wait until the program has both counts moving and its waiter waiting.
pub(super) fn wait_running(process: &Process) -> Checked<()> {
    until(
        process,
        &|| running(process),
        "the program never had both counts moving and its waiter waiting",
    )
}

/// The program's two counts.
pub(super) fn counts(process: &Process) -> Checked<(u32, u32)> {
    Ok((word(process, 0)?, word(process, 4)?))
}

/// Both counts moving and the waiter waiting.
fn running(process: &Process) -> Checked<bool> {
    Ok(try_word(process, 12) == Some(1)
        && futex::waiters_on(process, PAGE + 8) == 1
        && try_word(process, 0).is_some_and(|count| count != 0)
        && try_word(process, 4).is_some_and(|count| count != 0))
}

/// Require `cgroup.events` of `tail` to read exactly `expected`.
fn events(harness: &Harness, tail: &[u8], expected: &[u8], what: &'static str) -> Checked<()> {
    let mut path = Vec::from(tail);
    path.extend_from_slice(b"/cgroup.events");
    if harness.reads(&path, expected) {
        Ok(())
    } else {
        Err(what)
    }
}

/// Wait until `cgroup.events` of `tail` reads `expected`.
fn events_become(
    harness: &Harness,
    process: &Process,
    tail: &[u8],
    expected: &[u8],
    failure: &'static str,
) -> Checked<()> {
    until(
        process,
        &|| {
            let mut path = Vec::from(tail);
            path.extend_from_slice(b"/cgroup.events");
            Ok(harness.reads(&path, expected))
        },
        failure,
    )
}

/// Wait until the cgroup at `tail` is empty: a killed program's threads leave
/// its job when they have gone, not when the kill was sent.
pub(super) fn wait_empty(harness: &Harness, tail: &[u8]) -> Checked<()> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        let mut path = Vec::from(tail);
        path.extend_from_slice(b"/cgroup.events");
        let text = harness.read(&path).unwrap_or_default();
        if text.starts_with(b"populated 0") {
            return Ok(());
        }
        if crate::timer::now_nanos() >= deadline {
            return Err("freeze check: a killed program's cgroup never emptied");
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
}

/// Wait until `cgroup.events` of `tail` reads `expected`, for a cgroup whose
/// program has ended.
fn events_settle(
    harness: &Harness,
    tail: &[u8],
    expected: &[u8],
    failure: &'static str,
) -> Checked<()> {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    loop {
        if events(harness, tail, expected, failure).is_ok() {
            return Ok(());
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(failure);
        }
        crate::sched::sleep_for(POLL_NANOS);
    }
}

/// Run it. How many checks of a freeze passed: the boot line's number.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §11 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u32> {
    if arch::USER_STOPPED_PROGRAM.is_empty() {
        return Ok(0);
    }
    harness
        .mkdir(b"/check-fz")
        .map_err(|_| "freeze check: mkdir of a cgroup failed")?;
    harness.report.made += 1;
    let outcome = freeze_a_program(harness).and_then(|first| {
        let second = kill_a_frozen_program(harness)?;
        let nested = nested_and_moved(harness)?;
        Ok(first + second + nested)
    });
    let emptied = freeze_check_wait(harness);
    let removed = harness.rmdir(b"/check-fz");
    let passed = outcome?;
    emptied?;
    removed.map_err(|_| "freeze check: the cgroup did not empty")?;
    Ok(passed)
}

/// The cgroup of the check is empty before it is removed.
fn freeze_check_wait(harness: &Harness) -> Checked<()> {
    wait_empty(harness, b"/check-fz")
}

/// Freeze, hold, `SIGCONT`, thaw, freeze, `cgroup.kill`.
///
/// Verifies: L.object.107, H.QUOTA.11
fn freeze_a_program(harness: &mut Harness) -> Checked<u32> {
    let program = start(harness, b"/check-fz")?;
    let process = &program.process;
    let job = process.job();
    if !harness.reads(b"/check-fz/cgroup.freeze", b"0\n") {
        return Err("a new cgroup's cgroup.freeze does not read 0");
    }
    until(
        process,
        &|| running(process),
        "the program never had both counts moving and its waiter waiting",
    )?;
    events(
        harness,
        b"/check-fz",
        b"populated 1\nfrozen 0\n",
        "a running cgroup's cgroup.events does not say populated 1, frozen 0",
    )?;
    let watched = harness
        .open_read(b"/check-fz/cgroup.events")
        .map_err(|_| "cgroup.events did not open")?;
    let _ = Harness::read_to_end(&watched).map_err(|_| "cgroup.events did not read")?;
    if watched.poll().priority {
        return Err("a cgroup.events just read, with nothing changed, polls POLLPRI");
    }

    let refused = harness.write(b"/check-fz/cgroup.freeze", b"2\n");
    harness.refused(
        refused.err(),
        Errno::ERANGE,
        "a cgroup.freeze of 2 was not refused as Linux refuses it",
    )?;
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"1\n")
        .map_err(|_| "cgroup.freeze refused a 1")?;
    if !harness.reads(b"/check-fz/cgroup.freeze", b"1\n") {
        return Err("cgroup.freeze does not read back the 1 written to it");
    }
    events_become(
        harness,
        process,
        b"/check-fz",
        b"populated 1\nfrozen 1\n",
        "cgroup.events never said frozen 1 for a frozen cgroup",
    )?;
    if !watched.poll().priority {
        return Err("cgroup.events did not poll POLLPRI when its cgroup froze");
    }
    if !job.frozen_seen() || !process.every_task_blocked() {
        return Err("a frozen cgroup's tasks are not all blocked");
    }
    let still = (word(process, 0)?, word(process, 4)?);
    crate::sched::sleep_for(STILL_NANOS);
    if (word(process, 0)?, word(process, 4)?) != still {
        return Err("a thread of a frozen cgroup went on counting");
    }

    kill::send(process, SIGCONT, Origin::Kernel);
    crate::sched::sleep_for(STILL_NANOS);
    if (word(process, 0)?, word(process, 4)?) != still || !process.core().is_frozen() {
        return Err("SIGCONT thawed a frozen cgroup's process");
    }
    events(
        harness,
        b"/check-fz",
        b"populated 1\nfrozen 1\n",
        "SIGCONT changed what a frozen cgroup's cgroup.events says",
    )?;

    thaw_and_kill(harness, process, still)
}

/// Thaw the frozen program, see it run, freeze it again and `cgroup.kill` it.
fn thaw_and_kill(harness: &mut Harness, process: &Process, still: (u32, u32)) -> Checked<u32> {
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"0\n")
        .map_err(|_| "cgroup.freeze refused a 0")?;
    events(
        harness,
        b"/check-fz",
        b"populated 1\nfrozen 0\n",
        "a thawed cgroup's cgroup.events still says frozen 1",
    )?;
    until(
        process,
        &|| Ok(running(process)? && word(process, 0)? != still.0 && word(process, 4)? != still.1),
        "a thawed cgroup's threads did not all run again",
    )?;
    crate::sched::sleep_for(STILL_NANOS);
    if word(process, 12)? != 1 {
        return Err("a FUTEX_WAIT frozen and thawed returned instead of being restarted");
    }

    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"1\n")
        .map_err(|_| "cgroup.freeze refused a second 1")?;
    events_become(
        harness,
        process,
        b"/check-fz",
        b"populated 1\nfrozen 1\n",
        "a cgroup frozen a second time never said frozen 1",
    )?;
    let _ = harness
        .write(b"/check-fz/cgroup.kill", b"1\n")
        .map_err(|_| "cgroup.kill refused a frozen cgroup")?;
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline) != Some(KILLED) {
        return Err("cgroup.kill did not end a frozen cgroup's program by SIGKILL");
    }
    events_settle(
        harness,
        b"/check-fz",
        b"populated 0\nfrozen 1\n",
        "a frozen cgroup its kill emptied does not say populated 0, frozen 1",
    )?;
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"0\n")
        .map_err(|_| "cgroup.freeze refused a 0 on an empty cgroup")?;
    Ok(3)
}

/// A frozen program that is sent `SIGKILL` dies.
fn kill_a_frozen_program(harness: &mut Harness) -> Checked<u32> {
    let program = start(harness, b"/check-fz")?;
    let process = &program.process;
    until(
        process,
        &|| running(process),
        "a second program never had both counts moving and its waiter waiting",
    )?;
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"1\n")
        .map_err(|_| "cgroup.freeze refused a 1")?;
    events_become(
        harness,
        process,
        b"/check-fz",
        b"populated 1\nfrozen 1\n",
        "a cgroup frozen for a SIGKILL never said frozen 1",
    )?;
    kill::send(process, SIGKILL, Origin::Kernel);
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    if process.wait_for_exit(deadline) != Some(KILLED) {
        return Err("a frozen process sent SIGKILL did not die of it");
    }
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"0\n")
        .map_err(|_| "cgroup.freeze refused a 0")?;
    Ok(1)
}

/// A cgroup beneath a frozen one, a move in and out, a fork, and a process
/// made in a frozen cgroup.
fn nested_and_moved(harness: &mut Harness) -> Checked<u32> {
    harness
        .mkdir(b"/check-fz/n")
        .map_err(|_| "freeze check: mkdir of a nested cgroup failed")?;
    harness
        .mkdir(b"/check-fz/m")
        .map_err(|_| "freeze check: mkdir of a second cgroup failed")?;
    harness.report.made += 2;
    let outcome = nested_checks(harness);
    let _ = harness.write(b"/check-fz/cgroup.freeze", b"0\n");
    let removed = harness
        .rmdir(b"/check-fz/n")
        .and_then(|()| harness.rmdir(b"/check-fz/m"));
    let passed = outcome?;
    removed.map_err(|_| "freeze check: a nested cgroup did not empty")?;
    Ok(passed)
}

/// The checks of [`nested_and_moved`].
fn nested_checks(harness: &mut Harness) -> Checked<u32> {
    let member =
        process::new_for_check().map_err(|_| "freeze check: no process for the nested cgroup")?;
    let listed = alloc::format!("{}\n", member.pid());
    let _ = harness
        .write(b"/check-fz/n/cgroup.procs", listed.as_bytes())
        .map_err(|_| "freeze check: a move into the nested cgroup failed")?;
    let _ = harness
        .write(b"/check-fz/cgroup.freeze", b"1\n")
        .map_err(|_| "cgroup.freeze refused a 1")?;
    if !harness.reads(b"/check-fz/n/cgroup.freeze", b"0\n") {
        return Err("a cgroup beneath a frozen one has a cgroup.freeze of its own that reads 1");
    }
    events(
        harness,
        b"/check-fz/n",
        b"populated 1\nfrozen 1\n",
        "a cgroup beneath a frozen one does not say frozen 1",
    )?;
    if !member.core().is_frozen() {
        return Err("a process beneath a frozen cgroup is not frozen");
    }
    // A fork made there starts frozen.
    let child = process::fork_for_check(&member)
        .map_err(|_| "freeze check: a fork of the nested cgroup's process failed")?;
    if !child.core().is_frozen() {
        return Err("a fork made in a frozen cgroup did not start frozen");
    }
    // And so does a process made in a job by `Process::new`, as a
    // `CLONE_INTO_CGROUP` child is.
    let job: Arc<Job> = member.job();
    let made = Core::new(
        AddressSpace::new().map_err(|_| "freeze check: no address space")?,
        0,
        job,
    )
    .map_err(|_| "freeze check: no process for the frozen cgroup")?;
    if !made.is_frozen() {
        return Err("a process made in a frozen cgroup did not start frozen");
    }
    drop(made);
    // Into it, frozen; out of it, thawed.
    let outside = process::new_for_check()
        .map_err(|_| "freeze check: no process to move into the frozen cgroup")?;
    if outside.core().is_frozen() {
        return Err("a process in the root cgroup is frozen");
    }
    let listed = alloc::format!("{}\n", outside.pid());
    let _ = harness
        .write(b"/check-fz/m/cgroup.procs", listed.as_bytes())
        .map_err(|_| "freeze check: a move into the frozen cgroup failed")?;
    if !outside.core().is_frozen() {
        return Err("a process moved into a frozen cgroup is not frozen");
    }
    let _ = harness
        .write(b"/cgroup.procs", listed.as_bytes())
        .map_err(|_| "freeze check: a move out of the frozen cgroup failed")?;
    if outside.core().is_frozen() {
        return Err("a process moved out of a frozen cgroup is still frozen");
    }
    process::kill(&member, crate::object::job::KILLED_STATUS);
    process::kill(&child, crate::object::job::KILLED_STATUS);
    process::kill(&outside, crate::object::job::KILLED_STATUS);
    Ok(4)
}
