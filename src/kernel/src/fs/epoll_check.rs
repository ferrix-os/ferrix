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
//!
//! And the ready list (`fs::epoll`'s module doc), which is the load's own
//! behaviour and so verification evidence of it, traced to no requirement:
//! a wait on 64 registrations with one written looks at no more than two
//! files; a wait asleep on 64 is ended by the write's wake, through the
//! registration's mark, not by its own looking again; a unix socket
//! registered before its `connect` is looked at again after it; a file
//! changed between a wait's hooking and its looking is reported (the
//! hook-before-look meet); and a registration whose hook is refused is
//! looked at by every wait and its set no longer trusted.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::socket::{AF_UNIX, SOCK_STREAM};
use ferrix_linux_abi::types::{
    AT_FDCWD, EPOLL_CLOEXEC, EPOLL_CTL_ADD, EPOLL_CTL_DEL, EPOLL_CTL_MOD, EPOLLET, EPOLLEXCLUSIVE,
    EPOLLHUP, EPOLLIN, EPOLLONESHOT, EPOLLOUT, F_GETFD, FD_CLOEXEC, MAP_ANONYMOUS, MAP_PRIVATE,
    O_DIRECTORY, O_RDONLY, PROT_READ, PROT_WRITE, SEEK_CUR, SEEK_SET,
};
use ferrix_vfs::Errno;

use crate::fs;
use crate::fs::epoll;
use crate::mm;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
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

/// Where a `struct sockaddr_un` is staged.
const AT_ADDRESS: u64 = 1024;

/// The abstract name the refused-gathering case listens on.
const GATHER_NAME: &[u8] = b"ferrix-epoll-check-gather";

/// The abstract name the ready list's `connect` case listens on.
const LISTEN_NAME: &[u8] = b"ferrix-epoll-check";

/// How many pipes the ready list's cases register.
const MANY: usize = 64;

/// Which of them is written.
const WRITTEN: usize = 40;

/// How long a ready-list waiter waits, in milliseconds: past the check's
/// patience, so that only a wake or the once-a-second rescan ends it.
const WAITER_MILLIS: i32 = 5_000;

/// How long a case gives a waiter to come back: under the set's
/// once-a-second rescan, so a wait it ended is told from a wake.
const WOKEN_WITHIN_NANOS: u64 = 600_000_000;

/// How long a case waits for a waiter to list itself or reach a meet.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// What a ready-list waiter task waits on: the process, the page and the set.
static WAITER_SUBJECT: SpinLock<Option<(Arc<Process>, u64, i32)>> = SpinLock::new(None);

/// What its `epoll_wait` answered.
static WAITER_ANSWER: SpinLock<Option<Result<usize, Errno>>> = SpinLock::new(None);

/// Woken when it answered.
static WAITER_DONE: WaitQueue = WaitQueue::new();

/// Whether the waiter arms the look meet for its first look.
static WAITER_MEETS: AtomicBool = AtomicBool::new(false);

/// An operation `epoll_ctl` does not have.
const UNKNOWN_OP: u32 = 99;

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Events waits delivered.
    pub(crate) events: u32,
    /// Calls refused as Linux refuses them.
    pub(crate) refusals: u32,
    /// Frames the second run cost.
    pub(crate) leaked: i64,
    /// Files a wait on [`MANY`] registrations with one written looked at.
    pub(crate) looked: u64,
}

/// What one run counts.
#[derive(Debug, Default)]
struct Counts {
    events: u32,
    refusals: u32,
    looked: u64,
}

/// Run it twice, measured on the second.
pub(crate) fn run() -> Result<Report, &'static str> {
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
        looked: counts.looked,
    })
}

/// One run: stage the page, check, and close whatever was opened.
fn check_once(process: &Arc<Process>) -> Result<Counts, &'static str> {
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
        .and_then(|()| check_wait_refusals(process, page, &mut counts))
        .and_then(|()| check_the_ready_list(process, page, &mut counts));
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

/// Pipes, as read and write ends.
type Pipes = Vec<(i32, i32)>;

/// What a waiter's `epoll_wait` answered, if it did.
type Answer = Option<Result<usize, Errno>>;

/// The ready list's cases.
fn check_the_ready_list(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    a_wait_looks_at_what_was_woken(process, page, counts)?;
    a_sleeping_wait_is_woken(process, page, counts)?;
    a_socket_is_looked_at_after_its_connect(process, page, counts)?;
    a_file_changed_after_its_hook_is_reported(process, page, counts)?;
    a_refused_hook_is_looked_at_every_wait(process, page)?;
    a_mark_that_found_no_room_still_wakes(process, page, counts)?;
    a_refused_gathering_distrusts(process, page)?;
    Ok(())
}

/// The epoll set a descriptor names.
fn set_of(process: &Process, set: i32) -> Result<Arc<epoll::Epoll>, &'static str> {
    let file = fd::file(process, set).map_err(|_| "the epoll set is gone")?;
    epoll::of(&file).ok_or("an epoll set is not one")
}

/// A set holding [`MANY`] pipes' read ends for `EPOLLIN`, each with its
/// index for a cookie, and the pipes.
fn many_pipes(process: &Process, page: u64) -> Result<(i32, Pipes), &'static str> {
    let set = create(process, 0)?;
    let mut pipes = Vec::new();
    for at in 0..MANY {
        let (reader, writer) = pipe_pair(process, page)?;
        pipes.push((reader, writer));
        if ctl(
            process,
            page,
            set,
            EPOLL_CTL_ADD,
            reader,
            EPOLLIN,
            at as u64,
        ) != Ok(0)
        {
            return Err("a pipe could not be added to a set of many");
        }
    }
    Ok((set, pipes))
}

/// Close a set and its pipes.
fn close_many(process: &Process, set: i32, pipes: &[(i32, i32)]) -> Result<(), &'static str> {
    for &(reader, writer) in pipes {
        closed(process, reader)?;
        closed(process, writer)?;
    }
    closed(process, set)
}

/// Read the byte a case wrote back out of a pipe.
fn take_byte(process: &Process, page: u64, reader: i32) -> Result<(), &'static str> {
    if file::sys_read(process, reader, page + AT_BYTE + 1, 1) != Ok(1) {
        return Err("the byte written into a pipe would not read back");
    }
    Ok(())
}

/// A wait on [`MANY`] registrations with one of them written looks at the
/// one woken and no more than one other, not at every registration.
/// Measured again if the set's once-a-second rescan fell in between.
fn a_wait_looks_at_what_was_woken(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let (set, pipes) = many_pipes(process, page)?;
    let inner = set_of(process, set)?;
    // The first wait looks at every new registration, and hooks each.
    if !wait(process, page, set, 8, 0)?.is_empty() {
        return Err("a set of quiet pipes reported something");
    }
    if inner.hooked() < MANY {
        return Err("a wait did not hook the queues of the pipes it looked at");
    }
    let (_, writer) = *pipes.get(WRITTEN).ok_or("no pipe to write")?;
    let (reader, _) = *pipes.get(WRITTEN).ok_or("no pipe to write")?;
    let mut looked = u64::MAX;
    for _ in 0..3 {
        let (looks, rescans) = (inner.looks(), inner.rescans());
        put_byte(process, page, writer)?;
        let got = wait(process, page, set, 8, 0)?;
        if got != [(EPOLLIN, WRITTEN as u64)] {
            return Err("a wait on a set of many did not report the one pipe written");
        }
        counts.events += 1;
        take_byte(process, page, reader)?;
        // The pipe drained: the next wait drops it from the ready list.
        if !wait(process, page, set, 8, 0)?.is_empty() {
            return Err("a drained pipe was still reported");
        }
        if inner.rescans() == rescans {
            looked = inner.looks().saturating_sub(looks);
            break;
        }
    }
    drop(inner);
    close_many(process, set, &pipes)?;
    counts.looked = looked;
    // One look to report it, one to drop it once drained.
    if looked > 2 {
        crate::console::println!(
            "  epoll    a wait on {MANY} with one written looked at {looked} files"
        );
        return Err("a wait looked at registrations nothing woke");
    }
    Ok(())
}

/// A wait asleep on [`MANY`] registrations is ended by a write's wake --
/// counted on the set's queue, which the pipe's queue wakes through the
/// registration's mark -- within the case's patience, not by the rescan.
fn a_sleeping_wait_is_woken(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let (set, pipes) = many_pipes(process, page)?;
    let inner = set_of(process, set)?;
    if !wait(process, page, set, 8, 0)?.is_empty() {
        return Err("a set of quiet pipes reported something");
    }
    let (reader, writer) = *pipes.get(WRITTEN).ok_or("no pipe to write")?;
    let ended_before = inner.queue().waits_ended_by_a_wake();
    let (answer, took) = with_a_waiter(process, page, set, &inner, None, || {
        put_byte(process, page, writer)
    })?;
    let ended = inner.queue().waits_ended_by_a_wake() != ended_before;
    take_byte(process, page, reader)?;
    drop(inner);
    close_many(process, set, &pipes)?;
    if answer != Some(Ok(1)) {
        return Err("a wait asleep on a set of many did not report the pipe written");
    }
    if !ended || took >= WOKEN_WITHIN_NANOS {
        return Err(
            "a wait asleep on a set of many was ended by looking again, not by the write's wake",
        );
    }
    counts.events += 1;
    Ok(())
}

/// Start a waiter on `set` -- on another processor when `on` names one --
/// wait until it is listed on the set's queue (or, with a look meet armed,
/// at the meet), run `act`, and answer what its `epoll_wait` answered and
/// how long after `act` it came back.
fn with_a_waiter(
    process: &Arc<Process>,
    page: u64,
    set: i32,
    inner: &epoll::Epoll,
    on: Option<usize>,
    act: impl FnOnce() -> Result<(), &'static str>,
) -> Result<(Answer, u64), &'static str> {
    *WAITER_ANSWER.lock() = None;
    *WAITER_SUBJECT.lock() = Some((Arc::clone(process), page, set));
    // The waiter arms the meet for itself before its first look, so it is
    // that look, not a later one, that the meet holds.
    WAITER_MEETS.store(on.is_some(), Ordering::Release);
    let waiter = match on {
        Some(cpu) => crate::sched::spawn_on(
            "epoll-meet-waiter",
            ready_list_waiter,
            0,
            ferrix_sched::NICE_0_WEIGHT,
            cpu,
            ferrix_sched::CpuSet::of(cpu),
        )?,
        None => crate::sched::spawn(
            "epoll-ready-waiter",
            ready_list_waiter,
            0,
            ferrix_sched::NICE_0_WEIGHT,
        )?,
    };
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let there = || {
        if on.is_some() {
            epoll::look_meet_arrived()
        } else {
            inner.queue().listed() != 0
        }
    };
    while !there() && WAITER_ANSWER.lock().is_none() && crate::timer::now_nanos() < deadline {
        crate::sched::sleep_for(1_000_000);
    }
    let arrived = there();
    let acted = act();
    if on.is_some() {
        epoll::release_look_meet();
    }
    let started = crate::timer::now_nanos();
    let patience = started.saturating_add(PATIENCE_NANOS);
    let _ = WAITER_DONE.wait_until_deadline(|| WAITER_ANSWER.lock().is_some(), patience);
    let took = crate::timer::now_nanos().saturating_sub(started);
    let answer = WAITER_ANSWER.lock().take();
    *WAITER_SUBJECT.lock() = None;
    crate::sched::wait_until_gone(&waiter, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(waiter);
    acted?;
    if !arrived {
        return Err("a ready-list waiter never got to where its case acts");
    }
    Ok((answer, took))
}

/// The waiting task: one `epoll_wait` on what [`WAITER_SUBJECT`] names.
fn ready_list_waiter(_argument: usize) {
    if WAITER_MEETS.load(Ordering::Acquire)
        && let Some(me) = crate::sched::current()
    {
        epoll::arm_look_meet(me.id);
    }
    let subject = WAITER_SUBJECT.lock().clone();
    let answer = match subject {
        Some((process, page, set)) => {
            calls::sys_epoll_wait(&process, set, page + AT_EVENTS, 4, WAITER_MILLIS)
        }
        None => Err(Errno::ESRCH),
    };
    *WAITER_ANSWER.lock() = Some(answer);
    WAITER_DONE.wake_all();
}

/// A unix stream socket registered edge-triggered for `EPOLLOUT` before it
/// connects is looked at again after the connect, which changes what it
/// reports (its hang-up goes) and which queues it names (its peer's): the
/// connect wakes its own queues, and the look hooks its peer's.
fn a_socket_is_looked_at_after_its_connect(
    process: &Process,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let socket = |process: &Process| {
        descriptor(
            by_number(
                process,
                Syscall::Socket,
                [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
            ),
            "a unix stream socket was refused",
        )
    };
    let mut address = [0_u8; 2 + 1 + LISTEN_NAME.len()];
    for (slot, byte) in address.iter_mut().zip(AF_UNIX.to_le_bytes()) {
        *slot = byte;
    }
    for (slot, byte) in address.iter_mut().skip(3).zip(LISTEN_NAME) {
        *slot = *byte;
    }
    uaccess::copy_to_user(process.space(), page + AT_ADDRESS, &address)
        .map_err(|_| "could not stage a socket address")?;
    let length = address.len() as u64;
    let listener = socket(process)?;
    if by_number(
        process,
        Syscall::Bind,
        [listener as u64, page + AT_ADDRESS, length, 0, 0, 0],
    ) != Ok(0)
        || by_number(process, Syscall::Listen, [listener as u64, 1, 0, 0, 0, 0]) != Ok(0)
    {
        return Err("a unix socket would not bind an abstract name and listen");
    }
    let client = socket(process)?;
    let set = create(process, 0)?;
    let inner = set_of(process, set)?;
    if ctl(
        process,
        page,
        set,
        EPOLL_CTL_ADD,
        client,
        EPOLLOUT | EPOLLET,
        0x50,
    ) != Ok(0)
    {
        return Err("a unix socket could not be added to a set");
    }
    let unconnected = wait(process, page, set, 4, 0)?;
    if unconnected != [(EPOLLOUT | EPOLLHUP, 0x50)] {
        return Err("an unconnected unix stream socket did not report EPOLLOUT and EPOLLHUP once");
    }
    if !wait(process, page, set, 4, 0)?.is_empty() {
        return Err("an edge-triggered unix socket reported twice with nothing happening");
    }
    let hooked = inner.hooked();
    if by_number(
        process,
        Syscall::Connect,
        [client as u64, page + AT_ADDRESS, length, 0, 0, 0],
    ) != Ok(0)
    {
        return Err("a unix stream socket would not connect to a listener");
    }
    let connected = wait(process, page, set, 4, 0)?;
    let hooked_after = inner.hooked();
    drop(inner);
    for opened in [set, client, listener] {
        closed(process, opened)?;
    }
    if connected != [(EPOLLOUT, 0x50)] {
        return Err("a unix socket registered before its connect was not looked at again after it");
    }
    if hooked_after <= hooked {
        return Err("a unix socket's peer queues were not hooked once it connected");
    }
    counts.events += 2;
    Ok(())
}

/// A file written while a wait is held between hooking its registration's
/// queues and looking at the file is reported by that wait, well before the
/// once-a-second rescan: the hook is in place before the look.
fn a_file_changed_after_its_hook_is_reported(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let here = crate::sched::current()
        .ok_or("the epoll check is not running as a task")?
        .cpu();
    let online = crate::smp::count();
    let Some(there) = (0..online).find(|&cpu| cpu != here) else {
        crate::console::println!(
            "  epoll    a file changed between hook and look: not run, one processor"
        );
        return Ok(());
    };
    let set = create(process, 0)?;
    let inner = set_of(process, set)?;
    let (reader, writer) = pipe_pair(process, page)?;
    // Added, and not yet looked at: the waiter's first look hooks it.
    if ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0x51) != Ok(0) {
        return Err("a pipe could not be added to a set");
    }
    let (answer, took) = with_a_waiter(process, page, set, &inner, Some(there), || {
        put_byte(process, page, writer)
    })?;
    take_byte(process, page, reader)?;
    drop(inner);
    for opened in [set, reader, writer] {
        closed(process, opened)?;
    }
    if answer != Some(Ok(1)) || took >= WOKEN_WITHIN_NANOS {
        return Err(
            "a file changed between a wait's hook and its look was missed until the rescan",
        );
    }
    counts.events += 1;
    Ok(())
}

/// A mark that found its ready list full -- told of a wake, so marked, but
/// not on the list -- is cleared by the next wait, which the overflow makes
/// look at everything: the next write to its file wakes a sleeping wait
/// again, rather than leaving it to the once-a-second rescan for good (the
/// consultant's B1, ledger line 2050).
fn a_mark_that_found_no_room_still_wakes(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    let set = create(process, 0)?;
    let inner = set_of(process, set)?;
    let (reader, writer) = pipe_pair(process, page)?;
    if ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0x53) != Ok(0) {
        return Err("a pipe could not be added to a set");
    }
    // Hooked by a first look, then the list's room taken up.
    if !wait(process, page, set, 4, 0)?.is_empty() {
        return Err("a quiet pipe reported something");
    }
    inner.crowd();
    put_byte(process, page, writer)?;
    // The overflow: everything looked at, the pipe reported.
    let overflowed = wait(process, page, set, 4, 0)?;
    take_byte(process, page, reader)?;
    if overflowed != [(EPOLLIN, 0x53)] {
        return Err("a pipe whose mark found its ready list full was not reported");
    }
    if !wait(process, page, set, 4, 0)?.is_empty() {
        return Err("a drained pipe was still reported");
    }
    let ended_before = inner.queue().waits_ended_by_a_wake();
    let (answer, took) = with_a_waiter(process, page, set, &inner, None, || {
        put_byte(process, page, writer)
    })?;
    let ended = inner.queue().waits_ended_by_a_wake() != ended_before;
    take_byte(process, page, reader)?;
    drop(inner);
    for opened in [set, reader, writer] {
        closed(process, opened)?;
    }
    if answer != Some(Ok(1)) || !ended || took >= WOKEN_WITHIN_NANOS {
        return Err("a pipe whose mark once found its ready list full no longer wakes a wait");
    }
    counts.events += 2;
    Ok(())
}

/// A wait with no memory to gather the queues of the registrations it must
/// ask -- here a unix socket that has just connected, whose queues are not
/// fixed and now include its peer's -- distrusts them for that wait, rather
/// than leaving them trusted with nothing hooked (the consultant's B2, ledger
/// line 2050); the next wait asks again, hooks them and trusts it.
fn a_refused_gathering_distrusts(process: &Process, page: u64) -> Result<(), &'static str> {
    let me = crate::sched::current().ok_or("the epoll check is not running as a task")?;
    let socket = |process: &Process| {
        descriptor(
            by_number(
                process,
                Syscall::Socket,
                [u64::from(AF_UNIX), u64::from(SOCK_STREAM), 0, 0, 0, 0],
            ),
            "a unix stream socket was refused",
        )
    };
    let mut address = [0_u8; 2 + 1 + GATHER_NAME.len()];
    for (slot, byte) in address.iter_mut().zip(AF_UNIX.to_le_bytes()) {
        *slot = byte;
    }
    for (slot, byte) in address.iter_mut().skip(3).zip(GATHER_NAME) {
        *slot = *byte;
    }
    uaccess::copy_to_user(process.space(), page + AT_ADDRESS, &address)
        .map_err(|_| "could not stage a socket address")?;
    let length = address.len() as u64;
    let listener = socket(process)?;
    if by_number(
        process,
        Syscall::Bind,
        [listener as u64, page + AT_ADDRESS, length, 0, 0, 0],
    ) != Ok(0)
        || by_number(process, Syscall::Listen, [listener as u64, 1, 0, 0, 0, 0]) != Ok(0)
    {
        return Err("a unix socket would not bind an abstract name and listen");
    }
    let client = socket(process)?;
    let set = create(process, 0)?;
    let inner = set_of(process, set)?;
    if ctl(process, page, set, EPOLL_CTL_ADD, client, EPOLLOUT, 0x54) != Ok(0) {
        return Err("a unix socket could not be added to a set");
    }
    let _ = wait(process, page, set, 4, 0)?;
    let trusted_once_hooked = inner.trusted();
    if by_number(
        process,
        Syscall::Connect,
        [client as u64, page + AT_ADDRESS, length, 0, 0, 0],
    ) != Ok(0)
    {
        return Err("a unix stream socket would not connect to a listener");
    }
    // The first fallible allocation of this wait is the gathering's room:
    // the connected socket names queues it has not hooked.
    crate::fallible::inject_once(me.id, 1, false);
    let _ = inner.ready(4);
    let _ = crate::fallible::stop_injecting();
    let distrusted = !inner.trusted();
    let _ = inner.ready(4);
    let trusted_again = inner.trusted();
    drop(inner);
    for opened in [set, client, listener] {
        closed(process, opened)?;
    }
    if !trusted_once_hooked {
        return Err("an unconnected unix socket was not trusted once its first wait hooked it");
    }
    if !distrusted {
        return Err("a wait with no memory to gather what to hook left its registrations trusted");
    }
    if !trusted_again {
        return Err(
            "a wait with memory did not trust again a registration a refused gathering distrusted",
        );
    }
    Ok(())
}

/// A registration whose hook is refused for want of memory is looked at by
/// every wait, and its set says it can no longer be trusted, so a wait on it
/// looks again every few milliseconds rather than once a second.
fn a_refused_hook_is_looked_at_every_wait(
    process: &Process,
    page: u64,
) -> Result<(), &'static str> {
    let me = crate::sched::current().ok_or("the epoll check is not running as a task")?;
    for nth in 1..=16 {
        let set = create(process, 0)?;
        let inner = set_of(process, set)?;
        let (reader, writer) = pipe_pair(process, page)?;
        if ctl(process, page, set, EPOLL_CTL_ADD, reader, EPOLLIN, 0x52) != Ok(0) {
            return Err("a pipe could not be added to a set");
        }
        crate::fallible::inject_once(me.id, nth, false);
        let _ = inner.ready(4);
        let _ = crate::fallible::stop_injecting();
        // A refused hook stays refused: the next wait, with memory, still
        // does not trust it (a refused gathering, nth 1, is hooked by it).
        let _ = inner.ready(4);
        let refused = !inner.trusted();
        let reported = if refused {
            put_byte(process, page, writer)?;
            let got = inner.ready(4).len();
            take_byte(process, page, reader)?;
            Some(got)
        } else {
            None
        };
        drop(inner);
        for opened in [set, reader, writer] {
            closed(process, opened)?;
        }
        match reported {
            Some(1) => return Ok(()),
            Some(_) => {
                return Err(
                    "a registration whose hook was refused was not looked at by the next wait",
                );
            }
            None => {}
        }
    }
    Err("a registration whose hook was refused was left trusted")
}
