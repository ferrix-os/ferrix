//! Time namespaces, proved at boot (`docs/NAMESPACES.md` §12.1): driven
//! through the system-call layer and procfs as a program's calls would be.
//!
//! A root process unshares a time namespace and, as a program does, writes
//! its offsets before anything is made in it, then forks a child that is. The
//! caller must stay where it was (`ns/time`), name the new namespace in
//! `ns/time_for_children`, and read its own clocks as before. The offsets
//! file must refuse malformed text (`EINVAL`), a clock that would go negative
//! (`ERANGE`) and, once the child exists, every write (`EACCES`); and the
//! first namespace's is never writable.
//!
//! The child reads `CLOCK_MONOTONIC`, its raw and coarse forms and
//! `CLOCK_BOOTTIME` shifted by the offsets, one negative and one positive
//! so that a sign error shows, and `CLOCK_REALTIME` not at all; `times`,
//! `sysinfo` and `/proc/uptime` agree with the boot-time clock. The other
//! way, an absolute `clock_nanosleep`, `timerfd_settime` and `FUTEX_WAIT_BITSET`
//! deadline given in the child's time must land where the host's counter is
//! that long from now, not at a time already past.
//!
//! On `Credentials`, without a process: `CAP_SYS_ADMIN` is needed to make a
//! namespace, `CAP_SYS_TIME` over its owner by the opener and by the writer
//! to write its offsets, and fake root in a user namespace owns the time
//! namespace made with it, holds no power over one the first namespace owns,
//! and is still not privileged to set the clock.
//!
//! Last, the vDSO: a child forked into another time namespace is given the
//! variant whose data page makes every function a system call, at the address
//! the parent's mapping was at, and its parent keeps its own.

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    CLOCK_BOOTTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_COARSE, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME,
    TFD_TIMER_ABSTIME,
};
use ferrix_vfs::Errno;

use crate::fs::mount_check::{Page, Report as Counts, Tally, by_number, page_for};
use crate::fs::namespace_check::{read_file, read_link, unshare};
use crate::fs::userns_check::{acting, write_to};
use crate::syscall::credentials::Credentials;
use crate::syscall::namespace::CLONE_NEWUSER;
use crate::syscall::process::{self, Process};
use crate::syscall::thread::Thread;
use crate::syscall::time::{self, TimeWidth};
use crate::syscall::timens::{self, CLONE_NEWTIME, TimeNamespace};
use crate::syscall::userns::{self, CAP_SYS_TIME};
use crate::syscall::{family, futex, timerfd, vdso};

/// Nanoseconds in a second.
const NANOS: i128 = 1_000_000_000;

/// The boot-time offset the check writes: a day.
const BOOT_OFFSET_SECONDS: i128 = 86_400;

/// How far ahead the absolute deadlines are set.
const AHEAD: u64 = 150_000_000;

/// `TIMER_ABSTIME`.
const ABSOLUTE: u64 = 1;

/// `FUTEX_WAIT_BITSET` with `FUTEX_PRIVATE_FLAG`.
const FUTEX_WAIT_BITSET_PRIVATE: u64 = 9 | 128;

/// The first namespace's link text.
const FIRST_LINK: &[u8] = b"time:[4026531834]";

/// What the check saw, for the boot line.
pub(crate) fn run() -> Result<Counts, &'static str> {
    let mut counts = Counts::default();
    let mut tally = Tally {
        report: &mut counts,
    };
    let caller = process::new_for_check().map_err(|_| "could not make the time check's process")?;
    let mut page = page_for(&caller)?;
    let offsets = made_and_written(&mut page, &mut tally)?;
    let mapped = vdso::map_into(caller.space(), false);
    let child = process::fork_for_check(&caller)
        .map_err(|_| "could not fork the time check's child into its namespace")?;
    let mut inside = page_for(&child)?;
    frozen_and_named(&mut page, &child, &mut tally)?;
    shifted_reads(&mut inside, &mut page, &offsets, &mut tally)?;
    boot_time_views(&mut inside, &mut tally)?;
    absolute_deadlines(&mut inside, &child, &mut tally)?;
    credentials(&mut tally)?;
    user_namespace_made_with(&mut tally)?;
    vdso_views(&caller, &child, mapped, &mut tally)?;
    Ok(counts)
}

/// What the check wrote as offsets, in nanoseconds.
struct Offsets {
    /// Added to the monotonic clocks.
    monotonic: i128,
    /// Added to the boot-time clock.
    boottime: i128,
}

/// Linux's text for offsets of these sizes: `"%-10s %10lld %9ld\n"` a line.
fn text(monotonic: i128, boottime: i128) -> String {
    let line = |name: &str, nanos: i128| {
        format!(
            "{name:<10} {:>10} {:>9}\n",
            nanos.div_euclid(NANOS),
            nanos.rem_euclid(NANOS)
        )
    };
    let mut out = line("monotonic", monotonic);
    out.push_str(&line("boottime", boottime));
    out
}

/// What the caller's link text names, without its prefix: the number.
fn number_of(link: &[u8]) -> &[u8] {
    link.split(|&byte| byte == b'[').nth(1).unwrap_or_default()
}

/// The clock `which`, in nanoseconds, as the page's process reads it.
fn clock(page: &mut Page<'_>, which: u32) -> Result<u64, &'static str> {
    time::sys_clock_gettime(
        page.process,
        u64::from(which),
        page.buffer(),
        TimeWidth::Wide,
    )
    .map(drop)
    .map_err(|_| "clock_gettime was refused")?;
    let bytes = page.read_back(16)?;
    let (seconds, nanos) = bytes.split_at(8);
    let seconds = u64::from_le_bytes(seconds.try_into().map_err(|_| "a short timespec")?);
    let nanos = u64::from_le_bytes(nanos.try_into().map_err(|_| "a short timespec")?);
    Ok(seconds * 1_000_000_000 + nanos)
}

/// The time line of `/proc/<pid>/ns/`, read as the page's process.
fn link(page: &mut Page<'_>, pid: u32, name: &str) -> Result<Vec<u8>, &'static str> {
    read_link(page, format!("/proc/{pid}/ns/{name}").as_bytes())
}

/// Count a check that needs no errno.
fn counted(tally: &mut Tally<'_>, holds: bool, problem: &'static str) -> Result<(), &'static str> {
    tally.report.calls += 1;
    if holds { Ok(()) } else { Err(problem) }
}

/// The first namespace cannot be written; `unshare` moves the children and
/// not the caller; a file's malformed text, a clock made negative and a good
/// write are each answered as Linux answers them.
fn made_and_written(page: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<Offsets, &'static str> {
    let process = page.process;
    let pid = process.pid();
    let path = format!("/proc/{pid}/timens_offsets");
    let first = link(page, pid, "time")?;
    counted(
        tally,
        first == FIRST_LINK,
        "/proc/<pid>/ns/time of the first namespace was not time:[4026531834]",
    )?;
    let zero = acting(pid, || read_file(page, path.as_bytes()))??
        .map_err(|_| "timens_offsets could not be read")?;
    counted(
        tally,
        zero == text(0, 0).as_bytes(),
        "the first namespace's offsets did not read as zero in Linux's layout",
    )?;
    tally.refused(
        write_to(page, path.as_bytes(), b"monotonic 10 0\n")?,
        Errno::EACCES,
        "the first time namespace's offsets were written",
    )?;

    tally.ok(
        unshare(process, CLONE_NEWTIME),
        "unshare(CLONE_NEWTIME) was refused to root",
    )?;
    counted(
        tally,
        link(page, pid, "time")? == first,
        "unshare(CLONE_NEWTIME) moved the caller's own clocks",
    )?;
    let children = link(page, pid, "time_for_children")?;
    counted(
        tally,
        children.starts_with(b"time_for_children:[") && number_of(&children) != number_of(&first),
        "unshare(CLONE_NEWTIME) did not name a new namespace for the caller's children",
    )?;

    let malformed: [(&[u8], &str); 5] = [
        (
            b"garbage\n",
            "a timens_offsets write of garbage was accepted",
        ),
        (
            b"monotonic 1 1000000000\n",
            "an offset with nanoseconds past a second was accepted",
        ),
        (
            b"monotonic 1 2\nboottime 1 2\nmonotonic 1 2\n",
            "three lines were accepted",
        ),
        (
            b"realtime 1 2\n",
            "an offset for CLOCK_REALTIME was accepted",
        ),
        (b"", "an empty timens_offsets write was accepted"),
    ];
    for (data, problem) in malformed {
        tally.refused(
            write_to(page, path.as_bytes(), data)?,
            Errno::EINVAL,
            problem,
        )?;
    }
    tally.refused(
        write_to(page, path.as_bytes(), b"monotonic 9223372037 0\n")?,
        Errno::ERANGE,
        "an offset past KTIME_SEC_MAX was accepted",
    )?;
    let now = i128::from(crate::timer::now_nanos());
    tally.refused(
        write_to(
            page,
            path.as_bytes(),
            format!("monotonic -{} 0\n", now / NANOS + 5).as_bytes(),
        )?,
        Errno::ERANGE,
        "an offset that makes a clock negative was accepted",
    )?;

    // A negative monotonic offset (so that a sign error shows) and a day of
    // boot time. Half the clock, to stay inside its range.
    let monotonic = -(now / 2);
    let boottime = BOOT_OFFSET_SECONDS * NANOS;
    let given = text(monotonic, boottime);
    let short = format!(
        "monotonic {} {}\nboottime {BOOT_OFFSET_SECONDS} 0\n",
        monotonic.div_euclid(NANOS),
        monotonic.rem_euclid(NANOS)
    );
    tally.ok(
        write_to(page, path.as_bytes(), short.as_bytes())?,
        "a good write to timens_offsets was refused",
    )?;
    let read = acting(pid, || read_file(page, path.as_bytes()))??
        .map_err(|_| "timens_offsets could not be read back")?;
    counted(
        tally,
        read == given.as_bytes(),
        "timens_offsets did not read back as written, in Linux's layout",
    )?;
    Ok(Offsets {
        monotonic,
        boottime,
    })
}

/// A process made in the namespace freezes it, and is in its parent's
/// `time_for_children`.
fn frozen_and_named(
    page: &mut Page<'_>,
    child: &Arc<Process>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let pid = page.process.pid();
    let path = format!("/proc/{pid}/timens_offsets");
    let parents = link(page, pid, "time_for_children")?;
    let own = link(page, child.pid(), "time")?;
    let theirs = link(page, child.pid(), "time_for_children")?;
    counted(
        tally,
        number_of(&own) == number_of(&parents) && number_of(&theirs) == number_of(&parents),
        "a child was not made in its parent's time_for_children",
    )?;
    counted(
        tally,
        link(page, pid, "time")? == FIRST_LINK,
        "making a child moved its parent's own clocks",
    )?;
    tally.refused(
        write_to(page, path.as_bytes(), b"boottime 5 0\n")?,
        Errno::EACCES,
        "offsets stayed writable after a process was made in the namespace",
    )
}

/// `clock_gettime` in the child, clock by clock, against the host's counter
/// read either side.
fn shifted_reads(
    inside: &mut Page<'_>,
    outside: &mut Page<'_>,
    offsets: &Offsets,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let cases: [(u32, i128, &str); 4] = [
        (
            CLOCK_MONOTONIC,
            offsets.monotonic,
            "CLOCK_MONOTONIC was not shifted by the monotonic offset",
        ),
        (
            CLOCK_MONOTONIC_RAW,
            offsets.monotonic,
            "CLOCK_MONOTONIC_RAW was not shifted by the monotonic offset",
        ),
        (
            CLOCK_MONOTONIC_COARSE,
            offsets.monotonic,
            "CLOCK_MONOTONIC_COARSE was not shifted by the monotonic offset",
        ),
        (
            CLOCK_BOOTTIME,
            offsets.boottime,
            "CLOCK_BOOTTIME was not shifted by the boot-time offset",
        ),
    ];
    for (which, offset, problem) in cases {
        let before = i128::from(crate::timer::now_nanos());
        let read = i128::from(clock(inside, which)?);
        let after = i128::from(crate::timer::now_nanos());
        counted(
            tally,
            (before + offset..=after + offset).contains(&read),
            problem,
        )?;
    }
    let before = i128::from(crate::timer::now_nanos());
    let own = i128::from(clock(outside, CLOCK_MONOTONIC)?);
    let after = i128::from(crate::timer::now_nanos());
    counted(
        tally,
        (before..=after).contains(&own),
        "the caller of unshare(CLONE_NEWTIME) read CLOCK_MONOTONIC shifted",
    )?;
    // The real-time clock is the same in every namespace.
    let inner = i128::from(clock(inside, CLOCK_REALTIME)?);
    let outer = i128::from(clock(outside, CLOCK_REALTIME)?);
    counted(
        tally,
        (inner - outer).abs() < NANOS,
        "CLOCK_REALTIME was shifted by a time namespace",
    )
}

/// `times`, `sysinfo` and `/proc/uptime` read the boot-time clock.
fn boot_time_views(inside: &mut Page<'_>, tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let day = BOOT_OFFSET_SECONDS as u64;
    let ticks = by_number(inside.process, Syscall::Times, [0; 6])
        .map_err(|_| "times was refused in the time check")?;
    counted(
        tally,
        ticks as u64 >= day * 100,
        "times did not count the boot-time offset",
    )?;
    crate::syscall::system::sys_sysinfo(inside.process, inside.buffer())
        .map(drop)
        .map_err(|_| "sysinfo was refused in the time check")?;
    let word = inside.read_back(size_of::<usize>())?;
    let mut padded = [0_u8; 8];
    padded
        .get_mut(..word.len())
        .ok_or("a wide word")?
        .copy_from_slice(&word);
    counted(
        tally,
        u64::from_le_bytes(padded) >= day,
        "sysinfo's uptime did not count the boot-time offset",
    )?;
    let pid = inside.process.pid();
    let uptime = acting(pid, || read_file(inside, b"/proc/uptime"))??
        .map_err(|_| "/proc/uptime could not be read")?;
    let seconds = uptime
        .split(|&byte| byte == b'.')
        .next()
        .and_then(|digits| core::str::from_utf8(digits).ok())
        .and_then(|digits| digits.parse::<u64>().ok())
        .ok_or("/proc/uptime was malformed")?;
    counted(
        tally,
        seconds >= day,
        "/proc/uptime did not count the boot-time offset",
    )
}

/// An absolute time given in the child's clock lands `AHEAD` from now on the
/// host's: a sleep, a timerfd and a futex wait each take their time, where a
/// deadline read as the host's would be long past (the monotonic offset is
/// negative).
fn absolute_deadlines(
    inside: &mut Page<'_>,
    child: &Arc<Process>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let thread = Thread::leader(child).map_err(|_| "no thread for the time check's sleep")?;
    let staged = |page: &mut Page<'_>, nanos: u64| -> Result<u64, &'static str> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(nanos / 1_000_000_000).to_le_bytes());
        bytes.extend_from_slice(&(nanos % 1_000_000_000).to_le_bytes());
        page.reset();
        page.put_bytes(&bytes)
    };
    for (which, problem) in [
        (
            CLOCK_MONOTONIC,
            "an absolute clock_nanosleep on CLOCK_MONOTONIC returned at once: the deadline was not converted",
        ),
        (
            CLOCK_BOOTTIME,
            "an absolute clock_nanosleep on CLOCK_BOOTTIME returned at once: the deadline was not converted",
        ),
    ] {
        let deadline = clock(inside, which)? + AHEAD;
        let at = staged(inside, deadline)?;
        let began = crate::timer::now_nanos();
        tally.ok(
            time::sys_clock_nanosleep(&thread, which as i32, ABSOLUTE, [at, 0], TimeWidth::Wide),
            "an absolute clock_nanosleep was refused",
        )?;
        counted(
            tally,
            crate::timer::now_nanos().saturating_sub(began) >= AHEAD / 10,
            problem,
        )?;
    }

    // A timerfd: armed at an absolute time AHEAD from the child's now, the
    // time left must be about that, not zero.
    let fd = timerfd::sys_timerfd_create(inside.process, CLOCK_MONOTONIC as i32, 0)
        .map_err(|_| "timerfd_create was refused")?;
    let deadline = clock(inside, CLOCK_MONOTONIC)? + AHEAD;
    let mut setting = Vec::new();
    for value in [0_u64, 0, deadline / 1_000_000_000, deadline % 1_000_000_000] {
        setting.extend_from_slice(&value.to_le_bytes());
    }
    inside.reset();
    let at = inside.put_bytes(&setting)?;
    tally.ok(
        timerfd::sys_timerfd_settime(
            inside.process,
            fd as i32,
            TFD_TIMER_ABSTIME,
            [at, 0],
            TimeWidth::Wide,
        ),
        "an absolute timerfd_settime was refused",
    )?;
    tally.ok(
        timerfd::sys_timerfd_gettime(inside.process, fd as i32, inside.buffer(), TimeWidth::Wide),
        "timerfd_gettime was refused",
    )?;
    let left = inside.read_back(32)?;
    let field = |index: usize| {
        left.get(index * 8..index * 8 + 8)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map_or(0, u64::from_le_bytes)
    };
    let remaining = field(2) * 1_000_000_000 + field(3);
    counted(
        tally,
        remaining > 0 && remaining <= AHEAD,
        "an absolute timerfd on CLOCK_MONOTONIC was not converted to the host's counter",
    )?;
    let _ = by_number(inside.process, Syscall::Close, [fd as u64, 0, 0, 0, 0, 0]);

    // A futex wait with an absolute deadline on CLOCK_MONOTONIC.
    let deadline = clock(inside, CLOCK_MONOTONIC)? + AHEAD;
    let timeout = staged(inside, deadline)?;
    let word = inside.buffer() + 4096;
    let began = crate::timer::now_nanos();
    let waited = userns::acting_as(child, || {
        futex::sys_futex(
            &**child,
            &[word, FUTEX_WAIT_BITSET_PRIVATE, 0, timeout, 0, 0xffff_ffff],
            TimeWidth::Wide,
        )
    })?;
    tally.refused(
        waited,
        Errno::ETIMEDOUT,
        "a FUTEX_WAIT_BITSET did not time out",
    )?;
    counted(
        tally,
        crate::timer::now_nanos().saturating_sub(began) >= AHEAD / 10,
        "an absolute FUTEX_WAIT_BITSET returned at once: the deadline was not converted",
    )
}

/// Credentials, without a process: who may make a namespace, who may write
/// its offsets, and what a user namespace's fake root has over it.
fn credentials(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let root = Credentials::root();
    let mut user = Credentials::root();
    for ids in [&mut user.user, &mut user.group] {
        ids.real = 1000;
        ids.effective = 1000;
        ids.saved = 1000;
        ids.filesystem = 1000;
    }
    tally.refused(
        timens::create(&user, None).map(|_| 0),
        Errno::EPERM,
        "unshare(CLONE_NEWTIME) was allowed without CAP_SYS_ADMIN",
    )?;
    let namespace = timens::create(&root, None).map_err(|_| "root could not make a namespace")?;
    tally.refused(
        timens::write_offsets(&namespace, &user, &root, b"boottime 5 0\n"),
        Errno::EPERM,
        "a timens_offsets file opened by an unprivileged process was written by root",
    )?;
    tally.refused(
        timens::write_offsets(&namespace, &root, &user, b"boottime 5 0\n"),
        Errno::EPERM,
        "a timens_offsets file opened by root was written by an unprivileged process",
    )?;
    tally.ok(
        timens::write_offsets(&namespace, &root, &root, b"boottime 5 0\n"),
        "root could not write the offsets of a namespace it made",
    )?;

    // Fake root: a namespace made with a user namespace is owned by it.
    let fresh = userns::create(&user).map_err(|_| "a user namespace could not be made")?;
    let owned = timens::create(&user, Some(&fresh)).map_err(|_| "no namespace with a user one")?;
    let mut inside = user.clone();
    inside.user_ns = Arc::clone(&fresh);
    inside.caps = userns::CapSets::FRESH;
    tally.ok(
        timens::write_offsets(&owned, &inside, &inside, b"boottime 5 0\n"),
        "fake root could not write the offsets of the time namespace its user namespace owns",
    )?;
    tally.ok(
        timens::write_offsets(&owned, &user, &user, b"boottime 6 0\n"),
        "the owner outside a user namespace could not write the offsets of its time namespace",
    )?;
    tally.refused(
        timens::write_offsets(&namespace, &inside, &inside, b"boottime 7 0\n"),
        Errno::EPERM,
        "fake root wrote the offsets of a time namespace the first user namespace owns",
    )?;
    counted(
        tally,
        !inside.privileged() && inside.holds(CAP_SYS_TIME),
        "fake root was privileged to set the clock, or lacked CAP_SYS_TIME in its own namespace",
    )
}

/// `unshare(CLONE_NEWUSER | CLONE_NEWTIME)`, user namespace first: the
/// namespace is owned by the new one, and its creator can write the offsets.
fn user_namespace_made_with(tally: &mut Tally<'_>) -> Result<(), &'static str> {
    let maker = process::new_for_check().map_err(|_| "could not make the user-and-time process")?;
    let mut page = page_for(&maker)?;
    let pid = maker.pid();
    tally.ok(
        unshare(&maker, CLONE_NEWUSER | CLONE_NEWTIME),
        "unshare(CLONE_NEWUSER | CLONE_NEWTIME) was refused to root",
    )?;
    let owned: Arc<TimeNamespace> = maker.time_namespace_for_children();
    counted(
        tally,
        !owned.is_first() && !owned.owner().is_first(),
        "a time namespace made with a user namespace was not owned by it",
    )?;
    let path = format!("/proc/{pid}/timens_offsets");
    tally.ok(
        write_to(&mut page, path.as_bytes(), b"boottime 3600 0\n")?,
        "the creator of a user and a time namespace could not write the offsets",
    )?;
    counted(
        tally,
        owned.offset(timens::Shift::Boottime) == 3_600 * 1_000_000_000,
        "the creator's write to timens_offsets did not reach the namespace",
    )
}

/// The vDSO a forked child maps: the variant whose data page makes every
/// function a system call, at its parent's address, when its namespace is
/// not its parent's; and the parent's own view kept.
fn vdso_views(
    parent: &Arc<Process>,
    child: &Arc<Process>,
    mapped: Option<u64>,
    tally: &mut Tally<'_>,
) -> Result<(), &'static str> {
    let Some(mapped) = mapped else {
        // No vDSO on this architecture: nothing for a process to read.
        return Ok(());
    };
    let before = vdso::mapped_views(child.space());
    counted(
        tally,
        before == [Some(mapped - ferrix_bootinfo::PAGE_SIZE), None],
        "a forked space did not keep its parent's vDSO view where it was",
    )?;
    family::retarget_vdso(parent, child).map_err(|_| "the child's vDSO could not be swapped")?;
    let after = vdso::mapped_views(child.space());
    counted(
        tally,
        after == [None, before[0]],
        "a child forked into a time namespace kept the unshifted vDSO, or moved it",
    )?;
    counted(
        tally,
        vdso::shifted_mode() == Some(ferrix_vdso::MODE_SYSCALL),
        "the shifted vDSO's data page did not say system call",
    )?;
    counted(
        tally,
        vdso::mapped_views(parent.space()) == before,
        "swapping the child's vDSO changed its parent's",
    )?;
    // A grandchild in its parent's namespace keeps the parent's view.
    let grandchild = process::fork_for_check(child).map_err(|_| "could not fork for the vDSO")?;
    family::retarget_vdso(child, &grandchild).map_err(|_| "the vDSO decision failed")?;
    counted(
        tally,
        vdso::mapped_views(grandchild.space()) == after,
        "a child in its parent's time namespace was given another vDSO",
    )
}
