//! Stage 13's `cpu.max`, `cpu.stat` and `cpu.weight.nice`, landing C
//! (`docs/CGROUPS.md` §12).
//!
//! The files first: `cpu.max` reads `max 100000`, takes `quota period` and a
//! quota alone (keeping the period), refuses under a millisecond and what is
//! no number as Linux does; `cpu.weight.nice` and `cpu.weight` turn into each
//! other as the scheduler's table has them; `cpu.stat` has the three usage
//! keys in every cgroup and the three period keys where the `cpu`
//! controller is on.
//!
//! Then a program of three threads (`arch::USER_STOPPED_PROGRAM`, two of
//! which never stop counting) runs in a cgroup with `cpu.max` 20000 100000.
//! Across a window it must be given about a fifth of a processor, not the
//! two its threads would take: `cpu.stat`'s `usage_usec` moves by a fifth of
//! the wall clock, `nr_throttled` and `throttled_usec` count, `nr_periods`
//! counts, and the threads still count, a little. The same program in a
//! cgroup beneath one with that limit is held to it too, and with `max` back
//! the program takes more than half a processor again.

use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::SIGKILL;

use super::freeze_check::{self, Running};
use super::{Checked, Harness};
use crate::syscall::kill;
use crate::syscall::signal::Origin;

/// How long each measured window lasts: 600 ms.
const WINDOW_NANOS: u64 = 600_000_000;

/// Run it. How many periods the quota throttled, across the check.
///
/// # Errors
///
/// The first thing that was not as `docs/CGROUPS.md` §12 has it, by name.
pub(super) fn run(harness: &mut Harness) -> Checked<u64> {
    let _ = harness
        .write(b"/cgroup.subtree_control", b"+cpu\n")
        .map_err(|_| "the root refused to enable cpu for the cpu check")?;
    harness
        .mkdir(b"/check-c")
        .map_err(|_| "cpu check: mkdir of a cgroup failed")?;
    harness.report.made += 1;
    let outcome = files(harness).and_then(|()| throttling(harness));
    let emptied = freeze_check::wait_empty(harness, b"/check-c");
    let removed = harness.rmdir(b"/check-c");
    let disabled = harness.write(b"/cgroup.subtree_control", b"-cpu\n");
    let throttled = outcome?;
    emptied?;
    removed.map_err(|_| "cpu check: the cgroup did not empty")?;
    let _ = disabled.map_err(|_| "the root refused to disable cpu after the cpu check")?;
    Ok(throttled)
}

/// The number after `key` in the file at `path`.
fn number(harness: &Harness, path: &[u8], key: &str) -> Checked<u64> {
    let text = harness
        .read(path)
        .map_err(|_| "cpu check: a file did not read")?;
    let text = core::str::from_utf8(&text).map_err(|_| "cpu check: a file is not text")?;
    text.lines()
        .find_map(|line| {
            let (name, value) = line.split_once(' ')?;
            (name == key).then(|| value.parse().ok()).flatten()
        })
        .ok_or("cpu check: a key is missing from cpu.stat")
}

/// `usage_usec`, `user_usec` and `system_usec` from one read of `path`.
fn usage_split(harness: &Harness, path: &[u8]) -> Checked<(u64, u64, u64)> {
    let text = harness
        .read(path)
        .map_err(|_| "cpu check: a file did not read")?;
    let text = core::str::from_utf8(&text).map_err(|_| "cpu check: a file is not text")?;
    let key = |wanted: &str| {
        text.lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == wanted).then(|| value.parse().ok()).flatten()
            })
            .ok_or("cpu check: a key is missing from cpu.stat")
    };
    Ok((key("usage_usec")?, key("user_usec")?, key("system_usec")?))
}

/// How many lines the file at `path` has.
fn lines(harness: &Harness, path: &[u8]) -> Checked<usize> {
    let text = harness
        .read(path)
        .map_err(|_| "cpu check: a file did not read")?;
    Ok(text.iter().filter(|&&byte| byte == b'\n').count())
}

/// The files.
fn files(harness: &mut Harness) -> Checked<()> {
    if !harness.reads(b"/check-c/cpu.max", b"max 100000\n")
        || !harness.reads(b"/check-c/cpu.weight", b"100\n")
        || !harness.reads(b"/check-c/cpu.weight.nice", b"0\n")
    {
        return Err(
            "a new cgroup's cpu.max, cpu.weight and cpu.weight.nice do not read max 100000, 100, 0",
        );
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"20000 100000\n")
        .map_err(|_| "cpu.max refused 20000 100000")?;
    if !harness.reads(b"/check-c/cpu.max", b"20000 100000\n") {
        return Err("cpu.max does not read back 20000 100000");
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"30000")
        .map_err(|_| "cpu.max refused a quota alone")?;
    if !harness.reads(b"/check-c/cpu.max", b"30000 100000\n") {
        return Err("a cpu.max written with a quota alone did not keep its period");
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"max 200000")
        .map_err(|_| "cpu.max refused max and a period")?;
    if !harness.reads(b"/check-c/cpu.max", b"max 200000\n") {
        return Err("cpu.max does not read back max 200000");
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"max 100000")
        .map_err(|_| "cpu.max refused max 100000")?;
    for (bad, what) in [
        (
            &b"999 100000"[..],
            "cpu.max took a quota under a millisecond",
        ),
        (b"50000 999", "cpu.max took a period under a millisecond"),
        (b"50000 1000001", "cpu.max took a period over a second"),
        (b"lots", "cpu.max took a quota that is no number"),
    ] {
        let refused = harness.write(b"/check-c/cpu.max", bad);
        harness.refused(refused.err(), Errno::EINVAL, what)?;
    }
    if !harness.reads(b"/check-c/cpu.max", b"max 100000\n") {
        return Err("a refused cpu.max write changed cpu.max");
    }

    let _ = harness
        .write(b"/check-c/cpu.weight.nice", b"5\n")
        .map_err(|_| "cpu.weight.nice refused 5")?;
    if !harness.reads(b"/check-c/cpu.weight", b"33\n")
        || !harness.reads(b"/check-c/cpu.weight.nice", b"5\n")
    {
        return Err("cpu.weight.nice 5 is not cpu.weight 33, or does not read back as 5");
    }
    let refused = harness.write(b"/check-c/cpu.weight.nice", b"20");
    harness.refused(
        refused.err(),
        Errno::ERANGE,
        "a cpu.weight.nice of 20 was not ERANGE",
    )?;
    let _ = harness
        .write(b"/check-c/cpu.weight", b"100")
        .map_err(|_| "cpu.weight refused 100")?;
    if !harness.reads(b"/check-c/cpu.weight.nice", b"0\n") {
        return Err("cpu.weight 100 does not read as cpu.weight.nice 0");
    }

    if lines(harness, b"/check-c/cpu.stat")? != 6 {
        return Err("cpu.stat with the cpu controller on does not have its six keys");
    }
    if lines(harness, b"/cpu.stat")? != 6 || number(harness, b"/cpu.stat", "usage_usec")? == 0 {
        return Err("the root's cpu.stat does not have its six keys and the machine's usage");
    }
    harness
        .mkdir(b"/check-c/d")
        .map_err(|_| "cpu check: mkdir of a nested cgroup failed")?;
    harness.report.made += 1;
    let bare = lines(harness, b"/check-c/d/cpu.stat");
    let removed = harness.rmdir(b"/check-c/d");
    if bare? != 3 {
        return Err("cpu.stat without the cpu controller does not have just its three usage keys");
    }
    removed.map_err(|_| "cpu check: the nested cgroup did not go")?;
    Ok(())
}

/// Microseconds of processor time `path`'s cpu.stat says its cgroup used.
fn usage(harness: &Harness, path: &[u8]) -> Checked<u64> {
    number(harness, path, "usage_usec")
}

/// What a cgroup used across a window, in thousandths of the wall clock.
fn share(harness: &Harness, stat: &[u8]) -> Checked<u64> {
    let (before, start) = (usage(harness, stat)?, crate::timer::now_nanos());
    crate::sched::sleep_for(WINDOW_NANOS);
    let (after, end) = (usage(harness, stat)?, crate::timer::now_nanos());
    let wall = (end - start) / 1000;
    Ok(after.saturating_sub(before).saturating_mul(1000) / wall.max(1))
}

/// A program in a cgroup with a quota, and in a cgroup beneath one.
///
/// Verifies: L.object.106
fn throttling(harness: &mut Harness) -> Checked<u64> {
    if crate::arch::USER_STOPPED_PROGRAM.is_empty() {
        return Ok(0);
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"20000 100000\n")
        .map_err(|_| "cpu.max refused 20000 100000")?;
    let program = freeze_check::start(harness, b"/check-c")?;
    let outcome = measured(harness, &program);
    kill::send(&program.process, SIGKILL, Origin::Kernel);
    let deadline = crate::timer::now_nanos().saturating_add(10_000_000_000);
    let _ = program.process.wait_for_exit(deadline);
    let throttled = outcome?;
    let _ = harness.write(b"/check-c/cpu.max", b"max 100000\n");
    let killed = killed_throttled(harness)?;
    let beneath = beneath(harness)?;
    Ok(throttled + killed + beneath)
}

/// How long a program throttled for most of a second may take to end after
/// a `SIGKILL`: a few ticks, where waiting out its period is nine tenths of a
/// second.
const KILL_NANOS: u64 = 250_000_000;

/// A program whose job has used its quota is throttled until its period's end,
/// and a `SIGKILL` for it ends it at once, not then (a kill by the cgroup or
/// the OOM killer reaches it the same way). The quota is a millisecond in a
/// second, so the wait a kill that was not heard would cost is long.
///
/// Verifies: L.sched.3, H.QUOTA.10
fn killed_throttled(harness: &mut Harness) -> Checked<u64> {
    if crate::arch::USER_STOPPED_PROGRAM.is_empty() {
        return Ok(0);
    }
    let _ = harness
        .write(b"/check-c/cpu.max", b"1000 1000000\n")
        .map_err(|_| "cpu.max refused 1000 1000000")?;
    let program = freeze_check::start(harness, b"/check-c")?;
    let ready = freeze_check::wait_running(&program.process);
    // It has used its millisecond and is asleep until the period's end.
    crate::sched::sleep_for(100_000_000);
    let throttled = number(harness, b"/check-c/cpu.stat", "nr_throttled");
    let asked = crate::timer::now_nanos();
    kill::send(&program.process, SIGKILL, Origin::Kernel);
    let deadline = asked.saturating_add(10_000_000_000);
    let _ = program.process.wait_for_exit(deadline);
    let took = crate::timer::now_nanos().saturating_sub(asked);
    drop(program);
    let _ = harness.write(b"/check-c/cpu.max", b"max 100000\n");
    let emptied = freeze_check::wait_empty(harness, b"/check-c");
    ready?;
    if throttled? == 0 {
        return Err("a program under cpu.max 1000 1000000 was not throttled by its first second");
    }
    if took > KILL_NANOS {
        crate::console::println!(
            "  cpu      a SIGKILL took {} ms to end a throttled program",
            took / 1_000_000
        );
        return Err(
            "a program throttled by cpu.max 1000 1000000 waited out its period to die of SIGKILL",
        );
    }
    emptied?;
    Ok(1)
}

/// The measurements of [`throttling`]: held to a fifth, counted, and free.
fn measured(harness: &Harness, program: &Running) -> Checked<u64> {
    freeze_check::wait_running(&program.process)?;
    crate::sched::sleep_for(150_000_000);
    let counted = freeze_check::counts(&program.process)?;
    let held = share(harness, b"/check-c/cpu.stat")?;
    if !(80..=400).contains(&held) {
        crate::console::println!("  cpu      held to {held} thousandths of the wall clock");
        return Err(
            "a program under cpu.max 20000 100000 was not held to about a fifth of a processor",
        );
    }
    if freeze_check::counts(&program.process)? == counted {
        return Err("a program under cpu.max stopped counting altogether");
    }
    let throttled = number(harness, b"/check-c/cpu.stat", "nr_throttled")?;
    let periods = number(harness, b"/check-c/cpu.stat", "nr_periods")?;
    if throttled < 3 || periods < throttled {
        return Err("cpu.stat did not count the periods the quota ran out in");
    }
    if number(harness, b"/check-c/cpu.stat", "throttled_usec")? == 0 {
        return Err("cpu.stat did not count the time its cgroup spent throttled");
    }
    // From one read of the file: the program is still running, so three
    // reads would each be of a later instant.
    let (usage, user, system) = usage_split(harness, b"/check-c/cpu.stat")?;
    if user == 0 || user + system != usage {
        return Err("cpu.stat's user and system time are not the usage it reports");
    }

    let _ = harness
        .write(b"/check-c/cpu.max", b"max 100000\n")
        .map_err(|_| "cpu.max refused max")?;
    crate::sched::sleep_for(150_000_000);
    let free = share(harness, b"/check-c/cpu.stat")?;
    if free < 600 {
        crate::console::println!("  cpu      free at {free} thousandths of the wall clock");
        return Err("a program let off its cpu.max did not take more than half a processor");
    }
    Ok(throttled)
}

/// The same program in a cgroup beneath one with a quota.
fn beneath(harness: &mut Harness) -> Checked<u64> {
    harness
        .mkdir(b"/check-c/k")
        .map_err(|_| "cpu check: mkdir of a nested cgroup failed")?;
    harness.report.made += 1;
    let _ = harness
        .write(b"/check-c/cpu.max", b"20000 100000\n")
        .map_err(|_| "cpu.max refused 20000 100000")?;
    let program = freeze_check::start(harness, b"/check-c/k")?;
    let outcome = held_beneath(harness, &program);
    kill::send(&program.process, SIGKILL, Origin::Kernel);
    let deadline = crate::timer::now_nanos().saturating_add(10_000_000_000);
    let _ = program.process.wait_for_exit(deadline);
    drop(program);
    let _ = harness.write(b"/check-c/cpu.max", b"max 100000\n");
    let emptied = freeze_check::wait_empty(harness, b"/check-c/k");
    let removed = harness.rmdir(b"/check-c/k");
    let throttled = outcome?;
    emptied?;
    removed.map_err(|_| "cpu check: the nested cgroup did not go")?;
    Ok(throttled)
}

/// The measurement of [`beneath`].
fn held_beneath(harness: &Harness, program: &Running) -> Checked<u64> {
    freeze_check::wait_running(&program.process)?;
    crate::sched::sleep_for(150_000_000);
    let held = share(harness, b"/check-c/cpu.stat")?;
    if !(80..=400).contains(&held) {
        crate::console::println!(
            "  cpu      beneath, held to {held} thousandths of the wall clock"
        );
        return Err("a program beneath a cgroup with cpu.max 20000 100000 was not held to it");
    }
    let mut own = Vec::new();
    own.extend_from_slice(b"/check-c/k/cpu.stat");
    if usage(harness, &own)? == 0 {
        return Err("a cgroup's cpu.stat counts nothing of the program in it");
    }
    number(harness, b"/check-c/cpu.stat", "nr_throttled")
}
