//! The self-check of epoll: what a Wayland server's event loop asks of it.
//!
//! A set is made by number, as a program makes one, and watches pipes. A
//! level-triggered registration reports an unread pipe at every wait; an
//! edge-triggered one reports it once, and again only after more is written
//! -- the level-triggered registration beside it on the same pipe is the
//! control that the second wait was not simply blind. A one-shot registration
//! reports once and is quiet until `EPOLL_CTL_MOD` arms it again. With room
//! for one event, two ready pipes are reported by turns. A registration
//! outlives its number while a `dup` holds the file, and goes with the file.
//!
//! Each registration reports only the events it asked for: `EPOLLIN`, not the
//! `EPOLLRDNORM` a pipe's readiness also carries.
//!
//! Nesting: a set holding another is readable, and reports the inner set's
//! cookie, when the inner set has something to report; a set added to itself
//! is `EINVAL`, one added to a set it holds is `ELOOP`, and a chain of six
//! sets is `ELOOP` where five are not.
//!
//! And the refusals, each as Linux answers it: flags `epoll_create1` does not
//! take, a size `epoll_create` does not take, a read at an offset (where
//! `lseek` answers 0, as Linux's `noop_llseek` does), `EEXIST`, `ENOENT`,
//! `EPERM` for a directory, `EINVAL` for a descriptor that is not a set, an
//! unknown operation and `EPOLLEXCLUSIVE` misused, `EBADF`, `EFAULT` for an event that
//! cannot be read but not for `EPOLL_CTL_DEL`, which reads none, and for the
//! wait: `maxevents` before the buffer, the buffer before the descriptor, and a
//! signal set of the wrong size before anything. It runs twice and counts
//! frames across the second run.

use alloc::vec::Vec;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    AT_FDCWD, EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLET, EPOLLEXCLUSIVE,
    EPOLLIN, EPOLLONESHOT, EPOLLOUT, F_GETFD, FD_CLOEXEC, MAP_ANONYMOUS, MAP_PRIVATE, O_DIRECTORY,
    O_RDONLY, PROT_READ, PROT_WRITE, SEEK_CUR, SEEK_SET,
};
use ferrix_vfs::Errno;

use crate::fs;
use crate::fs::epoll;
use crate::mm;
use crate::syscall::check as syscall_check;
use crate::syscall::epoll::{self as calls, EVENT_BYTES};
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::{fd, file, fsctl, pipe, registry, thread, uaccess};

/// Where `pipe2` writes its two descriptors.
const AT_FDS: u64 = 0;
/// Where the event `epoll_ctl` reads is staged.
const AT_EVENT: u64 = 16;
/// The byte written into pipes.
const AT_BYTE: u64 = 40;
/// `/tmp`, a directory, which epoll refuses.
const AT_TMP: u64 = 48;
/// A `struct timespec` of two 64-bit words, for `epoll_pwait2`.
const AT_TIMESPEC: u64 = 64;
/// Where waits write their events.
const AT_EVENTS: u64 = 128;
/// Where `fstatfs` writes, room for the widest layout.
const AT_STATFS: u64 = 512;

/// `/tmp`, terminated.
const TMP: &[u8] = b"/tmp\0";

/// An address no program may write.
const KERNEL_ADDRESS: u64 = u64::MAX - 0xFFF;

/// An operation `epoll_ctl` does not have.
const UNKNOWN_OP: u32 = 99;

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    /// Events waits delivered.
    pub events: u32,
    /// Calls refused as Linux refuses them.
    pub refusals: u32,
    /// Frames the second run cost.
    pub leaked: i64,
}

/// What one run counts.
#[derive(Debug, Default)]
struct Counts {
    events: u32,
    refusals: u32,
}

/// Run it twice, measured on the second.
pub fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the epoll check")?;
    let _warm = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let counts = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("epoll");
        crate::console::println!("  epoll    {leaked} frames across the second run");
        return Err("the epoll check did not give back every frame it took");
    }
    Ok(Report {
        events: counts.events,
        refusals: counts.refusals,
        leaked,
    })
}

/// One run: stage the page, check, and close whatever was opened.
fn check_once(process: &Process) -> Result<Counts, &'static str> {
    let page = memory::sys_mmap(
        process,
        &MmapRequest {
            addr: 0,
            len: PAGE_SIZE,
            prot: PROT_READ | PROT_WRITE,
            flags: MAP_ANONYMOUS | MAP_PRIVATE,
            fd: -1,
            offset: 0,
            unit: OffsetUnit::Bytes,
        },
    )
    .map_err(|_| "a page for the epoll check was refused")?;
    let page = u64::try_from(page).map_err(|_| "mmap returned an impossible address")?;
    let mut counts = Counts::default();
    let outcome = stage(process, page)
        .and_then(|()| check_creation(process, page, &mut counts))
        .and_then(|()| check_levels_edges_and_one_shots(process, page, &mut counts))
        .and_then(|()| check_turns_and_closing(process, page, &mut counts))
        .and_then(|()| check_nesting(process, page, &mut counts))
        .and_then(|()| check_refusals(process, page, &mut counts))
        .and_then(|()| check_wait_refusals(process, page, &mut counts));
    for fd in 3..64 {
        let _ = fd::sys_close(process, fd);
    }
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome.map(|()| counts)
}

/// Write the constant parts of the page.
fn stage(process: &Process, page: u64) -> Result<(), &'static str> {
    for (offset, bytes) in [(AT_BYTE, b"e".as_slice()), (AT_TMP, TMP)] {
        uaccess::copy_to_user(process.space(), page + offset, bytes)
            .map_err(|_| "could not stage the epoll check")?;
    }
    Ok(())
}

/// `epoll_create1` and `epoll_create`, and the flags they take.
fn check_creation(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let set = create(process, EPOLL_CLOEXEC)?;
    if fd::sys_fcntl(process, set, F_GETFD, 0) != Ok(FD_CLOEXEC as usize) {
        return Err("epoll_create1's EPOLL_CLOEXEC did not reach the descriptor");
    }
    let plain = create(process, 0)?;
    if fd::sys_fcntl(process, plain, F_GETFD, 0) != Ok(0) {
        return Err("epoll_create1 without EPOLL_CLOEXEC set close-on-exec");
    }
    refused(
        by_number(process, Syscall::EpollCreate1, [1, 0, 0, 0, 0, 0]),
        Errno::EINVAL,
        "epoll_create1 accepted a flag it does not take",
        counts,
    )?;
    refused(
        calls::sys_epoll_create(process, 0),
        Errno::EINVAL,
        "epoll_create accepted a size of zero",
        counts,
    )?;
    let sized = descriptor(
        calls::sys_epoll_create(process, 1),
        "epoll_create with a size of one was refused",
    )?;
    if fsctl::sys_fstatfs(process, set, page + AT_STATFS) != Ok(0) {
        return Err("fstatfs on an epoll set was refused");
    }
    let mut magic = [0_u8; 4];
    uaccess::copy_from_user(process.space(), page + AT_STATFS, &mut magic)
        .map_err(|_| "could not read fstatfs's answer back")?;
    if u64::from(u32::from_le_bytes(magic)) != fs::anon::ANON_INODE_FS_MAGIC {
        return Err("fstatfs on an epoll set did not report ANON_INODE_FS_MAGIC");
    }
    // `lseek` is Linux's `noop_llseek`: 0, whatever it is asked. A read at an
    // offset is still refused, as Linux refuses it.
    if [(100, SEEK_SET), (-5, SEEK_CUR)]
        .into_iter()
        .any(|(offset, whence)| fd::sys_lseek(process, set, offset, whence) != Ok(0))
    {
        return Err("lseek on an epoll set did not answer 0, as Linux's does");
    }
    refused(
        file::sys_pread64(process, set, page + AT_EVENTS, 8, 0),
        Errno::ESPIPE,
        "an epoll set could be read at an offset",
        counts,
    )?;
    for opened in [set, plain, sized] {
        closed(process, opened)?;
    }
    Ok(())
}

/// Level, edge and one-shot registrations on pipes.
fn check_levels_edges_and_one_shots(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let level = create(process, 0)?;
    let edge = create(process, 0)?;
    let once = create(process, 0)?;
    let (reader, writer) = pipe_pair(process, page)?;
    for (set, flags, data) in [
        (level, 0, 0x1122_3344_5566_7788),
        (edge, EPOLLET, 0xE),
        (once, EPOLLONESHOT, 0x1),
    ] {
        let _ = ctl(
            process,
            page,
            set,
            EPOLL_CTL_ADD,
            reader,
            EPOLLIN | flags,
            data,
        )
        .map_err(|_| "a pipe's read end could not be added to a set")?;
    }
    if wait(process, page, level, 4, 0)? != Vec::new() {
        return Err("a set watching an empty pipe reported something");
    }

    put_byte(process, page, writer)?;
    let first = wait(process, page, level, 4, 0)?;
    if first != [(EPOLLIN, 0x1122_3344_5566_7788)] {
        return Err("a level-triggered set did not report the pipe's data with its cookie");
    }
    if wait(process, page, edge, 4, 0)? != [(EPOLLIN, 0xE)] {
        return Err("an edge-triggered set did not report a pipe that became readable");
    }
    if wait(process, page, once, 4, 0)? != [(EPOLLIN, 0x1)] {
        return Err("a one-shot set did not report a readable pipe");
    }
    counts.events += 3;

    // Nothing new has happened: level reports again, edge and one-shot do not.
    if wait(process, page, level, 4, 0)?.len() != 1 {
        return Err("a level-triggered set stopped reporting a pipe that is still readable");
    }
    if !wait(process, page, edge, 4, 0)?.is_empty() {
        return Err("an edge-triggered set reported a pipe again with nothing new written");
    }
    if !wait(process, page, once, 4, 0)?.is_empty() {
        return Err("a one-shot set reported a second time without EPOLL_CTL_MOD");
    }
    counts.events += 1;

    // Something new: edge reports; one-shot still does not until modified.
    put_byte(process, page, writer)?;
    if wait(process, page, edge, 4, 0)?.len() != 1 {
        return Err("an edge-triggered set did not report more data written into a readable pipe");
    }
    if !wait(process, page, once, 4, 0)?.is_empty() {
        return Err("a disarmed one-shot registration reported new data");
    }
    let _ = ctl(
        process,
        page,
        once,
        EPOLL_CTL_MOD,
        reader,
        EPOLLIN | EPOLLONESHOT,
        0x2,
    )
    .map_err(|_| "EPOLL_CTL_MOD was refused on a one-shot registration")?;
    if wait(process, page, once, 4, 0)? != [(EPOLLIN, 0x2)] {
        return Err("EPOLL_CTL_MOD did not arm a one-shot registration with its new cookie");
    }
    counts.events += 2;

    // Drained, then written again: the edge the classic loop depends on.
    if file::sys_read(process, reader, page + AT_EVENTS, 64) != Ok(2) {
        return Err("the check could not drain its pipe");
    }
    if !wait(process, page, edge, 4, 0)?.is_empty() {
        return Err("an edge-triggered set reported a drained pipe");
    }
    put_byte(process, page, writer)?;
    if wait(process, page, edge, 4, 0)?.len() != 1 {
        return Err("an edge-triggered set missed data written after the pipe was drained");
    }
    counts.events += 1;

    // A pipe's write end: writable, and hung up once its reader goes.
    let _ = ctl(process, page, level, EPOLL_CTL_ADD, writer, EPOLLOUT, 0xF)
        .map_err(|_| "a pipe's write end could not be added to a set")?;
    let both = wait(process, page, level, 4, 0)?;
    if !both.contains(&(EPOLLOUT, 0xF)) {
        return Err("a set did not report a pipe's write end writable");
    }
    counts.events += 1;
    for opened in [level, edge, once, reader, writer] {
        closed(process, opened)?;
    }
    Ok(())
}

/// Two ready pipes and room for one event are reported by turns; and a
/// registration lives as long as its file, not its number.
fn check_turns_and_closing(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let set = create(process, 0)?;
    let (first_reader, first_writer) = pipe_pair(process, page)?;
    let (second_reader, second_writer) = pipe_pair(process, page)?;
    for (reader, writer, data) in [
        (first_reader, first_writer, 1),
        (second_reader, second_writer, 2),
    ] {
        let _ = ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, data)
            .map_err(|_| "a pipe could not be added to a set")?;
        put_byte(process, page, writer)?;
    }
    let one = wait(process, page, set, 1, 0)?;
    let other = wait(process, page, set, 1, 0)?;
    let cookies = [
        one.first().map(|event| event.1),
        other.first().map(|event| event.1),
    ];
    if cookies != [Some(1), Some(2)] {
        return Err("a set with room for one event did not report two ready pipes by turns");
    }
    counts.events += 2;

    let copy = descriptor(fd::sys_dup(process, first_reader), "dup was refused")?;
    closed(process, first_reader)?;
    let kept = wait(process, page, set, 4, 0)?;
    if !kept.iter().any(|event| event.1 == 1) {
        return Err("closing a registered number removed a registration its dup keeps open");
    }
    counts.events += 1;
    closed(process, copy)?;
    let _ = wait(process, page, set, 4, 0)?;
    let registered = fd::file(process, set)
        .ok()
        .and_then(|opened| epoll::of(&opened))
        .map(|opened| opened.len());
    if registered != Some(1) {
        return Err("a registration outlived the last descriptor of its file");
    }
    for opened in [set, first_writer, second_reader, second_writer] {
        closed(process, opened)?;
    }
    Ok(())
}

/// A set inside a set.
fn check_nesting(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let outer = create(process, 0)?;
    let inner = create(process, 0)?;
    let (reader, writer) = pipe_pair(process, page)?;
    let _ = ctl(process, page, inner, EPOLL_CTL_ADD, reader, EPOLLIN, 0x10)
        .map_err(|_| "a pipe could not be added to the inner set")?;
    let _ = ctl(process, page, outer, EPOLL_CTL_ADD, inner, EPOLLIN, 0x20)
        .map_err(|_| "a set could not be added to another set")?;
    let outer_file = fd::file(process, outer).map_err(|_| "the outer set is gone")?;
    if outer_file.poll().readable || !wait(process, page, outer, 4, 0)?.is_empty() {
        return Err("a set holding a set with nothing to report was ready");
    }
    put_byte(process, page, writer)?;
    if !outer_file.poll().readable {
        return Err("a set holding a set with something to report did not poll readable");
    }
    if wait(process, page, outer, 4, 0)? != [(EPOLLIN, 0x20)] {
        return Err("a set holding a ready set did not report it with the inner set's cookie");
    }
    if wait(process, page, inner, 4, 0)? != [(EPOLLIN, 0x10)] {
        return Err("the inner set did not report the pipe after the outer set had");
    }
    counts.events += 2;

    refused(
        ctl(process, page, outer, EPOLL_CTL_ADD, outer, EPOLLIN, 0),
        Errno::EINVAL,
        "a set added to itself was not EINVAL",
        counts,
    )?;
    refused(
        ctl(process, page, inner, EPOLL_CTL_ADD, outer, EPOLLIN, 0),
        Errno::ELOOP,
        "a set added to a set it holds was not ELOOP",
        counts,
    )?;
    refused(
        ctl(
            process,
            page,
            outer,
            EPOLL_CTL_ADD,
            inner,
            EPOLLIN | EPOLLEXCLUSIVE,
            1,
        ),
        Errno::EINVAL,
        "EPOLLEXCLUSIVE on a set added to a set was not EINVAL",
        counts,
    )?;

    // A chain: five sets deep is allowed, six is not.
    let mut chain = Vec::new();
    for _ in 0..6 {
        chain.push(create(process, 0)?);
    }
    for pair in chain.windows(2).take(4) {
        let [holder, held] = [pair.first(), pair.get(1)];
        let (Some(&holder), Some(&held)) = (holder, held) else {
            return Err("the chain is short");
        };
        let _ = ctl(process, page, holder, EPOLL_CTL_ADD, held, EPOLLIN, 0)
            .map_err(|_| "a chain of five sets was refused")?;
    }
    let (Some(&fifth), Some(&sixth)) = (chain.get(4), chain.get(5)) else {
        return Err("the chain is short");
    };
    refused(
        ctl(process, page, fifth, EPOLL_CTL_ADD, sixth, EPOLLIN, 0),
        Errno::ELOOP,
        "a chain of six sets was not ELOOP",
        counts,
    )?;
    for opened in chain.into_iter().chain([outer, inner, reader, writer]) {
        closed(process, opened)?;
    }
    Ok(())
}

/// The refusals, in Linux's order.
fn check_refusals(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let set = create(process, 0)?;
    let (reader, writer) = pipe_pair(process, page)?;
    let _ = ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0)
        .map_err(|_| "a pipe could not be added to a set")?;
    let directory = descriptor(
        fd::sys_openat(process, AT_FDCWD, page + AT_TMP, O_RDONLY | O_DIRECTORY, 0),
        "/tmp would not open",
    )?;
    let cases: [(Result<usize, Errno>, Errno, &'static str); 9] = [
        (
            ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0),
            Errno::EEXIST,
            "adding a registered pipe again was not EEXIST",
        ),
        (
            ctl(process, page, set, EPOLL_CTL_MOD, writer, EPOLLIN, 0),
            Errno::ENOENT,
            "modifying an unregistered pipe was not ENOENT",
        ),
        (
            ctl(process, page, set, EPOLL_CTL_DEL, writer, 0, 0),
            Errno::ENOENT,
            "removing an unregistered pipe was not ENOENT",
        ),
        (
            ctl(process, page, set, EPOLL_CTL_ADD, directory, EPOLLIN, 0),
            Errno::EPERM,
            "adding a directory, which cannot be waited on, was not EPERM",
        ),
        (
            ctl(process, page, reader, EPOLL_CTL_ADD, writer, EPOLLIN, 0),
            Errno::EINVAL,
            "epoll_ctl on a descriptor that is not a set was not EINVAL",
        ),
        (
            ctl(process, page, set, UNKNOWN_OP, writer, EPOLLIN, 0),
            Errno::EINVAL,
            "an unknown epoll_ctl operation was not EINVAL",
        ),
        (
            ctl(process, page, set, EPOLL_CTL_ADD, 999, EPOLLIN, 0),
            Errno::EBADF,
            "adding a descriptor that names nothing was not EBADF",
        ),
        (
            ctl(
                process,
                page,
                set,
                EPOLL_CTL_ADD,
                writer,
                EPOLLOUT | EPOLLEXCLUSIVE | EPOLLONESHOT,
                0,
            ),
            Errno::EINVAL,
            "EPOLLEXCLUSIVE with EPOLLONESHOT was not EINVAL",
        ),
        (
            ctl(
                process,
                page,
                set,
                EPOLL_CTL_MOD,
                reader,
                EPOLLIN | EPOLLEXCLUSIVE,
                0,
            ),
            Errno::EINVAL,
            "EPOLL_CTL_MOD with EPOLLEXCLUSIVE was not EINVAL",
        ),
    ];
    for (got, wanted, what) in cases {
        refused(got, wanted, what, counts)?;
    }

    for opened in [set, reader, writer, directory] {
        closed(process, opened)?;
    }
    Ok(())
}

/// `EFAULT` for an event only where one is read; and the waits' refusals:
/// `maxevents` first, then the buffer, then the
/// descriptor; and for the two that take a thread, the signal set and the
/// timeout before anything.
fn check_wait_refusals(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let set = create(process, 0)?;
    let (reader, writer) = pipe_pair(process, page)?;
    let _ = ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0)
        .map_err(|_| "a pipe could not be added to a set")?;
    // An event is read only by the operations that take one.
    refused(
        by_number(
            process,
            Syscall::EpollCtl,
            [
                set as u64,
                u64::from(EPOLL_CTL_ADD),
                writer as u64,
                KERNEL_ADDRESS,
                0,
                0,
            ],
        ),
        Errno::EFAULT,
        "an event epoll_ctl could not read was not EFAULT",
        counts,
    )?;
    if by_number(
        process,
        Syscall::EpollCtl,
        [
            set as u64,
            u64::from(EPOLL_CTL_DEL),
            reader as u64,
            KERNEL_ADDRESS,
            0,
            0,
        ],
    ) != Ok(0)
    {
        return Err("EPOLL_CTL_DEL read the event it does not take");
    }

    let waits: [(Result<usize, Errno>, Errno, &'static str); 4] = [
        (
            calls::sys_epoll_wait(process, 999, page + AT_EVENTS, 0, 0),
            Errno::EINVAL,
            "a wait for zero events was not EINVAL before its descriptor was looked at",
        ),
        (
            calls::sys_epoll_wait(process, 999, KERNEL_ADDRESS, 1, 0),
            Errno::EFAULT,
            "a wait into kernel memory was not EFAULT before its descriptor was looked at",
        ),
        (
            calls::sys_epoll_wait(process, 999, page + AT_EVENTS, 1, 0),
            Errno::EBADF,
            "a wait on a descriptor that names nothing was not EBADF",
        ),
        (
            calls::sys_epoll_wait(process, reader, page + AT_EVENTS, 1, 0),
            Errno::EINVAL,
            "a wait on a descriptor that is not a set was not EINVAL",
        ),
    ];
    for (got, wanted, what) in waits {
        refused(got, wanted, what, counts)?;
    }

    // The two waits that take a thread: a wrong signal set size, and a
    // timespec whose nanoseconds are out of range.
    let leader = registry::find(process.pid())
        .map(|found| thread::Thread::leader(&found))
        .ok_or("the check's process was not findable by its pid")?
        .map_err(|_| "no memory for a check's thread")?;
    let events = page + AT_EVENTS;
    refused(
        calls::sys_epoll_pwait(&leader, [set as u64, events, 1, 0, page + AT_BYTE, 4]),
        Errno::EINVAL,
        "epoll_pwait accepted a signal set of the wrong size",
        counts,
    )?;
    let bad_timespec: Vec<u8> = 0_i64
        .to_le_bytes()
        .into_iter()
        .chain(2_000_000_000_i64.to_le_bytes())
        .collect();
    uaccess::copy_to_user(process.space(), page + AT_TIMESPEC, &bad_timespec)
        .map_err(|_| "could not stage a timespec")?;
    refused(
        calls::sys_epoll_pwait2(&leader, [set as u64, events, 1, page + AT_TIMESPEC, 0, 8]),
        Errno::EINVAL,
        "epoll_pwait2 accepted a timeout of two seconds' worth of nanoseconds",
        counts,
    )?;
    for opened in [set, reader, writer] {
        closed(process, opened)?;
    }
    Ok(())
}

/// `epoll_create1(flags)` by number, as a descriptor.
fn create(process: &Process, flags: u32) -> Result<i32, &'static str> {
    descriptor(
        by_number(
            process,
            Syscall::EpollCreate1,
            [u64::from(flags), 0, 0, 0, 0, 0],
        ),
        "epoll_create1 was refused",
    )
}

/// `epoll_ctl` by number, with the event staged in the page.
fn ctl(
    process: &Process,
    page: u64,
    set: i32,
    op: u32,
    target: i32,
    events: u32,
    data: u64,
) -> Result<usize, Errno> {
    uaccess::copy_to_user(
        process.space(),
        page + AT_EVENT,
        &calls::encode(events, data),
    )
    .map_err(|_| Errno::EFAULT)?;
    by_number(
        process,
        Syscall::EpollCtl,
        [
            set as u64,
            u64::from(op),
            target as u64,
            page + AT_EVENT,
            0,
            0,
        ],
    )
}

/// `epoll_wait` for at most `max` events and `timeout` milliseconds, as the
/// events and cookies it wrote.
fn wait(
    process: &Process,
    page: u64,
    set: i32,
    max: i32,
    timeout: i32,
) -> Result<Vec<(u32, u64)>, &'static str> {
    let count = calls::sys_epoll_wait(process, set, page + AT_EVENTS, max, timeout)
        .map_err(|_| "epoll_wait was refused")?;
    let mut bytes = alloc::vec![0_u8; count * EVENT_BYTES];
    uaccess::copy_from_user(process.space(), page + AT_EVENTS, &mut bytes)
        .map_err(|_| "could not read the events back")?;
    Ok(bytes
        .chunks_exact(EVENT_BYTES)
        .map(|event| {
            let mut events = [0_u8; 4];
            let mut data = [0_u8; 8];
            for (slot, byte) in events.iter_mut().zip(event.iter()) {
                *slot = *byte;
            }
            for (slot, byte) in data.iter_mut().zip(event.iter().skip(EVENT_BYTES - 8)) {
                *slot = *byte;
            }
            (u32::from_le_bytes(events), u64::from_le_bytes(data))
        })
        .collect())
}

/// A pipe's two ends.
fn pipe_pair(process: &Process, page: u64) -> Result<(i32, i32), &'static str> {
    if pipe::sys_pipe2(process, page + AT_FDS, 0) != Ok(0) {
        return Err("pipe2 was refused");
    }
    let mut bytes = [0_u8; 8];
    uaccess::copy_from_user(process.space(), page + AT_FDS, &mut bytes)
        .map_err(|_| "could not read pipe2's descriptors")?;
    let [a, b, c, d, e, f, g, h] = bytes;
    Ok((
        i32::from_le_bytes([a, b, c, d]),
        i32::from_le_bytes([e, f, g, h]),
    ))
}

/// Write one byte into a pipe.
fn put_byte(process: &Process, page: u64, writer: i32) -> Result<(), &'static str> {
    if file::sys_write(process, writer, page + AT_BYTE, 1) != Ok(1) {
        return Err("a byte would not go into a pipe");
    }
    Ok(())
}

/// Close a descriptor the check opened.
fn closed(process: &Process, opened: i32) -> Result<(), &'static str> {
    fd::sys_close(process, opened)
        .map(|_| ())
        .map_err(|_| "a descriptor the epoll check opened would not close")
}

/// Make `call` by its number, as a program would.
fn by_number(process: &Process, call: Syscall, args: [u64; 6]) -> Result<usize, Errno> {
    syscall_check::call_by_number(process, call, args)
}

/// A descriptor a call returned.
fn descriptor(got: Result<usize, Errno>, what: &'static str) -> Result<i32, &'static str> {
    got.ok().and_then(|fd| i32::try_from(fd).ok()).ok_or(what)
}

/// Require `got` to be `Err(wanted)`, counting the refusal.
fn refused(
    got: Result<usize, Errno>,
    wanted: Errno,
    what: &'static str,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    if got != Err(wanted) {
        return Err(what);
    }
    counts.refusals += 1;
    Ok(())
}
