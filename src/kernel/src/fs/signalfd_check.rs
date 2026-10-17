//! The self-check of signalfds.
//!
//! Made by number, a signalfd takes `SFD_NONBLOCK` and `SFD_CLOEXEC`, refuses
//! every other flag, a mask of the wrong size, a mask it cannot read, and a
//! descriptor to change that is no signalfd, and never reads `SIGKILL` or
//! `SIGSTOP`. With nothing pending it reads `EAGAIN` and does not poll
//! readable. A signal sent to the process and pending makes it readable, and a
//! read takes it -- its `signalfd_siginfo` names the signal, `SI_USER` and the
//! sender -- and leaves it pending no longer. A signal outside the mask is not
//! read and stays pending until `signalfd4` hands the descriptor a mask that
//! holds it. Two pending signals come back in one read, lowest first.
//!
//! The one that matters most: a blocked `read`, a `poll` and an `epoll_wait`,
//! each in a task of its own and each waiting before the signal is sent, are
//! ended by the wake the signal's arrival makes, not by their own one-second
//! recheck -- the wake count says which, and the time from the send to the
//! waiter coming back must stay under [`LATE_LIMIT_NANOS`], a quarter of that
//! recheck.
//!
//! Each of the three is timed up to [`ATTEMPTS`] times, and passes on the
//! first attempt that meets both, as the timerfd check's waiters do (FX-0883).
//! An emulator whose host stops it for a few hundred milliseconds between
//! the send and the waiter's return makes that attempt as late as the stop
//! is long: the guest's clock ran on while none of it did, which no kernel
//! can tell from a slow wake. Every attempt is judged on the same evidence
//! and printed, so a kernel that wakes late, or never wakes the queue, fails
//! all of them; only a stall gets another chance (FX-0884).
//!
//! Refused as Linux refuses: a read into fewer than 128 bytes, a write, and a
//! read at an offset. `lseek` answers 0, as Linux's `noop_llseek` does. The run
//! is done twice and must keep no frame.

use alloc::sync::Arc;

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_linux_abi::nr::Syscall;
use ferrix_linux_abi::types::{
    EPOLL_CTL_ADD, EPOLLIN, F_GETFD, F_GETFL, FD_CLOEXEC, MAP_ANONYMOUS, MAP_PRIVATE, O_NONBLOCK,
    PROT_READ, PROT_WRITE, SEEK_CUR, SEEK_SET, SFD_CLOEXEC, SFD_NONBLOCK, SIGALRM, SIGKILL,
    SIGNALFD_SIGINFO_BYTES, SIGSTOP, SIGUSR1, SIGUSR2,
};
use ferrix_vfs::{Errno, OpenFile};

use crate::fs::signalfd::{self, SignalFd};
use crate::mm;
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::syscall::check as syscall_check;
use crate::syscall::memory::{self, MmapRequest, OffsetUnit};
use crate::syscall::process::{self, Process};
use crate::syscall::signal::{Origin, bit};
use crate::syscall::signalfd as calls;
use crate::syscall::{epoll, fd, file, kill, poll, uaccess};

/// Where reads land: room for three `signalfd_siginfo`s.
const AT_INFO: u64 = 0;
/// Where a signal mask is staged.
const AT_MASK: u64 = 512;
/// Where a `pollfd` or an epoll event is staged.
const AT_EVENT: u64 = 576;
/// Where epoll waits write.
const AT_EVENTS: u64 = 640;

/// The pid the check's signals say they came from.
const SENDER: u32 = 4242;
/// Where the check's handlers are, as far as the dispositions say: nowhere a
/// program runs, since nothing is ever delivered. A handler rather than the
/// default, so that a signal sent to a process with no thread to block it is
/// kept pending rather than ending the process.
const HANDLER: u64 = 0x7000_0000;
/// `si_code` for `kill`.
const SI_USER: i32 = 0;

/// Nanoseconds in a millisecond.
const MILLI: u64 = 1_000_000;

/// The latest a waiter may come back after the signal is sent: a quarter of
/// the trusted recheck, as the timerfd check allows, so a waiter its recheck
/// ended cannot pass while a loaded host keeps a wide margin.
const LATE_LIMIT_NANOS: u64 = crate::fs::wake::TRUSTED_RECHECK_NANOS / 4;
/// How many times a waiter is timed before a late one is a failure: enough
/// that one stall of the host, which spoils the attempt it lands in, cannot
/// fail the check, and few enough that a kernel that is late every time is
/// not waited on for long.
const ATTEMPTS: u32 = 3;
/// How long the check gives a waiter to start waiting, and to come back.
const PATIENCE_NANOS: u64 = 2_000_000_000;
/// How often the check looks for a waiter on the queue.
const LISTED_LOOK_NANOS: u64 = MILLI;
/// The waiting `poll` or `epoll_wait`'s own timeout, far past the patience.
const WAITER_TIMEOUT_MILLIS: i32 = 10_000;

/// What the waiting task waits on, and how.
static WAITER: SpinLock<Option<Waiter>> = SpinLock::new(None);
/// What it answered, and when, on the counter.
static ANSWER: SpinLock<Option<(Result<usize, Errno>, u64)>> = SpinLock::new(None);
/// Woken when it has answered.
static DONE: WaitQueue = WaitQueue::new();

/// What a waiting task is to wait on.
struct Waiter {
    process: Arc<Process>,
    page: u64,
    /// The descriptor `poll` watches, or the epoll set `epoll_wait` waits on.
    target: i32,
    /// The signalfd's open file, which a read reads.
    file: Arc<OpenFile>,
    how: HowWaits,
}

/// Which call the waiting task makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HowWaits {
    /// A blocking `read`.
    Read,
    /// `poll` on the signalfd.
    Poll,
    /// `epoll_wait` on a set holding it.
    Epoll,
}

impl HowWaits {
    /// The call, as the boot log names it.
    const fn call(self) -> &'static str {
        match self {
            HowWaits::Read => "read",
            HowWaits::Poll => "poll",
            HowWaits::Epoll => "epoll_wait",
        }
    }
}

/// How one timed waiter came back after its signal was sent, on the counter.
#[derive(Debug, Clone, Copy)]
enum Timed {
    /// Ended by the signal's wake, within [`LATE_LIMIT_NANOS`].
    OnTime(u64),
    /// Ended by the signal's wake, but later than that.
    Late(u64),
    /// Ended by its own recheck, the wake never having reached it.
    Rechecked(u64),
}

impl Timed {
    /// How long after the send the waiter came back.
    const fn late(self) -> u64 {
        match self {
            Timed::OnTime(late) | Timed::Late(late) | Timed::Rechecked(late) => late,
        }
    }

    /// What ended the wait, for the line an attempt prints.
    const fn ended_by(self) -> &'static str {
        match self {
            Timed::OnTime(_) | Timed::Late(_) => "woken by the signal",
            Timed::Rechecked(_) => "ended by its own recheck",
        }
    }

    /// What an attempt that came back like this is a failure of, if every
    /// attempt did.
    const fn failure(self) -> &'static str {
        match self {
            Timed::OnTime(_) | Timed::Late(_) => {
                "a waiter on a signalfd came back too long after its signal"
            }
            Timed::Rechecked(_) => {
                "a waiter on a signalfd was ended by its recheck, not by the signal's wake"
            }
        }
    }
}

/// What the check measured, for the boot log.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report {
    /// Signals read back through signalfds.
    pub(crate) signals: u64,
    /// Calls refused as Linux refuses them.
    pub(crate) refusals: u32,
    /// The latest a waiter came back after its signal was sent, in
    /// microseconds.
    pub(crate) late_micros: u64,
    /// Frames the second run cost.
    pub(crate) leaked: i64,
}

/// What one run counts.
#[derive(Debug, Default)]
struct Counts {
    signals: u64,
    refusals: u32,
    late: u64,
}

/// Run it twice, measured on the second.
pub(crate) fn run() -> Result<Report, &'static str> {
    let process =
        process::new_for_check().map_err(|_| "could not make a process for the signalfd check")?;
    process.with_signals(|signals| {
        for signal in [SIGUSR1, SIGUSR2, SIGALRM] {
            signals.install_action(signal, HANDLER, 0);
        }
    });
    let _warm = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let window = mm::FrameWindow::open();
    let counts = check_once(&process)?;
    crate::sched::wait_until_reaper_quiet(crate::sched::REAPER_PATIENCE_NANOS)?;
    let leaked = window.kept();
    if leaked != 0 {
        window.report("signalfd");
        crate::console::println!("  signalfd {leaked} frames across the second run");
        return Err("the signalfd check did not give back every frame it took");
    }
    Ok(Report {
        signals: counts.signals,
        refusals: counts.refusals,
        late_micros: counts.late / 1_000,
        leaked,
    })
}

/// One run.
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
    .map_err(|_| "a page for the signalfd check was refused")?;
    let page = u64::try_from(page).map_err(|_| "mmap returned an impossible address")?;
    let mut counts = Counts::default();
    let outcome = check_refusals(process, page, &mut counts)
        .and_then(|()| check_reads(process, page, &mut counts))
        .and_then(|()| check_the_mask(process, page, &mut counts))
        .and_then(|()| check_waiters_are_woken(process, page, &mut counts));
    for fd in 3..32 {
        let _ = fd::sys_close(process, fd);
    }
    let _ = WAITER.lock().take();
    // Nothing the check sent may outlive it, whatever failed: a pending
    // signal ends every later wait of a process with no thread.
    let drained = drain(process, page);
    let _ = memory::sys_munmap(process, page, PAGE_SIZE);
    outcome.and(drained).map(|()| counts)
}

/// The flags, the mask, and every refusal that needs no signal.
fn check_refusals(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let made = create(process, page, u64::MAX, SFD_NONBLOCK | SFD_CLOEXEC)?;
    if fd::sys_fcntl(process, made, F_GETFD, 0) != Ok(FD_CLOEXEC as usize) {
        return Err("signalfd4's SFD_CLOEXEC did not reach the descriptor");
    }
    if fd::sys_fcntl(process, made, F_GETFL, 0).map(|flags| flags as u32 & O_NONBLOCK)
        != Ok(O_NONBLOCK)
    {
        return Err("signalfd4's SFD_NONBLOCK did not reach the open file");
    }
    let inner = of(process, made)?;
    if inner.mask() & (bit(SIGKILL) | bit(SIGSTOP)) != 0 {
        return Err("a signalfd took SIGKILL or SIGSTOP into its mask");
    }
    refused(
        read_infos(process, page, made, 1).map(|infos| infos.len()),
        Errno::EAGAIN,
        "a non-blocking signalfd with nothing pending did not answer EAGAIN",
        counts,
    )?;
    if opened(process, made)?.poll().readable {
        return Err("a signalfd with nothing pending polled readable");
    }
    stage_mask(process, page, bit(SIGUSR1))?;
    let minus_one = u64::MAX;
    let refusals: [(Syscall, [u64; 6], Errno, &'static str); 4] = [
        (
            Syscall::Signalfd4,
            [minus_one, page + AT_MASK, 4, 0, 0, 0],
            Errno::EINVAL,
            "signalfd4 took a mask of four bytes",
        ),
        (
            Syscall::Signalfd4,
            [minus_one, 0, 8, 0, 0, 0],
            Errno::EFAULT,
            "signalfd4 took a mask at address zero",
        ),
        (
            Syscall::Signalfd4,
            [minus_one, page + AT_MASK, 8, 1, 0, 0],
            Errno::EINVAL,
            "signalfd4 took a flag Linux refuses",
        ),
        (
            Syscall::Signalfd4,
            [1_000, page + AT_MASK, 8, 0, 0, 0],
            Errno::EBADF,
            "signalfd4 took a descriptor no table has",
        ),
    ];
    for (call, args, wanted, what) in refusals {
        refused(by_number(process, call, args), wanted, what, counts)?;
    }
    let not_a_signalfd = descriptor(
        by_number(process, Syscall::Eventfd2, [0, 0, 0, 0, 0, 0]),
        "eventfd2 was refused",
    )?;
    refused(
        by_number(
            process,
            Syscall::Signalfd4,
            [
                u64::from(not_a_signalfd.cast_unsigned()),
                page + AT_MASK,
                8,
                0,
                0,
                0,
            ],
        ),
        Errno::EINVAL,
        "signalfd4 changed the mask of a descriptor that is not a signalfd",
        counts,
    )?;
    closed(process, not_a_signalfd)?;
    check_file_refusals(process, page, made, counts)?;
    closed(process, made)
}

/// The older call, which takes no flags, and what a signalfd refuses as a
/// file: a short read, a write and a seek.
fn check_file_refusals(
    process: &Process,
    page: u64,
    made: i32,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    // The older call, where the architecture has it, through its handler.
    let old = descriptor(
        calls::sys_signalfd(process, -1, page + AT_MASK, 8),
        "signalfd was refused",
    )?;
    if fd::sys_fcntl(process, old, F_GETFD, 0) != Ok(0) {
        return Err("signalfd, which takes no flags, set close-on-exec");
    }
    closed(process, old)?;
    refused(
        file::sys_read(process, made, page + AT_INFO, 64),
        Errno::EINVAL,
        "a signalfd read into 64 bytes was not EINVAL",
        counts,
    )?;
    refused(
        file::sys_write(process, made, page + AT_INFO, 128),
        Errno::EINVAL,
        "a signalfd could be written",
        counts,
    )?;
    // `lseek` is Linux's `noop_llseek`: 0, whatever it is asked. A read at an
    // offset is still refused, as Linux refuses it.
    if [(100, SEEK_SET), (-5, SEEK_CUR)]
        .into_iter()
        .any(|(offset, whence)| fd::sys_lseek(process, made, offset, whence) != Ok(0))
    {
        return Err("lseek on a signalfd did not answer 0, as Linux's does");
    }
    refused(
        file::sys_pread64(process, made, page + AT_INFO, 128, 0),
        Errno::ESPIPE,
        "a signalfd could be read at an offset",
        counts,
    )
}

/// A signal sent and pending is readable, read with its sender, and gone;
/// two come back in one read, lowest first.
fn check_reads(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let made = create(process, page, bit(SIGUSR1) | bit(SIGUSR2), SFD_NONBLOCK)?;
    let file = opened(process, made)?;
    kill::send(
        process,
        SIGUSR1,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    if pending(process) & bit(SIGUSR1) == 0 {
        return Err("a signal sent to the check's process was not left pending");
    }
    if !file.poll().readable {
        return Err("a signalfd with a signal of its mask pending did not poll readable");
    }
    let infos = read_infos(process, page, made, 3)
        .map_err(|_| "a signalfd with a signal pending could not be read")?;
    match infos.as_slice() {
        [(SIGUSR1, SI_USER, SENDER)] => {}
        [_] => return Err("a signalfd read did not name the signal, SI_USER and its sender"),
        _ => return Err("a signalfd read with one signal pending did not answer one"),
    }
    counts.signals += 1;
    if pending(process) & bit(SIGUSR1) != 0 {
        return Err("a signal read through a signalfd was still pending");
    }
    refused(
        read_infos(process, page, made, 1).map(|infos| infos.len()),
        Errno::EAGAIN,
        "a signalfd read twice for one signal",
        counts,
    )?;

    kill::send(
        process,
        SIGUSR2,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    kill::send(
        process,
        SIGUSR1,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    let infos = read_infos(process, page, made, 3)
        .map_err(|_| "a signalfd with two signals pending could not be read")?;
    match infos.as_slice() {
        [(SIGUSR1, ..), (SIGUSR2, ..)] => {}
        _ => return Err("one read of a signalfd did not take both pending signals, lowest first"),
    }
    counts.signals += 2;
    drop(file);
    closed(process, made)
}

/// A signal outside the mask is left pending, and read once `signalfd4` hands
/// the descriptor a mask that holds it.
fn check_the_mask(process: &Process, page: u64, counts: &mut Counts) -> Result<(), &'static str> {
    let made = create(process, page, bit(SIGUSR2), SFD_NONBLOCK)?;
    kill::send(
        process,
        SIGALRM,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    kill::send(
        process,
        SIGUSR2,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    let infos = read_infos(process, page, made, 3)
        .map_err(|_| "a signalfd with a signal of its mask pending could not be read")?;
    if !matches!(infos.as_slice(), [(SIGUSR2, ..)]) {
        return Err("a signalfd read a signal outside its mask");
    }
    counts.signals += 1;
    if pending(process) & bit(SIGALRM) == 0 {
        return Err("a signal outside a signalfd's mask did not stay pending");
    }
    stage_mask(process, page, bit(SIGALRM))?;
    if by_number(
        process,
        Syscall::Signalfd4,
        [u64::from(made.cast_unsigned()), page + AT_MASK, 8, 0, 0, 0],
    ) != usize::try_from(made).map_err(|_| Errno::EBADF)
    {
        return Err("signalfd4 would not change a signalfd's mask");
    }
    let infos = read_infos(process, page, made, 3)
        .map_err(|_| "a signalfd given a new mask could not be read")?;
    if !matches!(infos.as_slice(), [(SIGALRM, SI_USER, SENDER)]) {
        return Err("a signalfd given a new mask did not read the signal it now holds");
    }
    counts.signals += 1;
    if pending(process) != 0 {
        return Err("a signal the mask check sent was still pending after it");
    }
    closed(process, made)
}

/// A blocked read, a `poll` and an `epoll_wait`, each waiting in a task of its
/// own before the signal is sent, are ended by the wake its arrival makes --
/// on one of [`ATTEMPTS`] attempts each, for the reason the module gives.
fn check_waiters_are_woken(
    process: &Arc<Process>,
    page: u64,
    counts: &mut Counts,
) -> Result<(), &'static str> {
    for how in [HowWaits::Read, HowWaits::Poll, HowWaits::Epoll] {
        let mut attempt = 1;
        let late = loop {
            let timed = time_a_waiter(process, page, how)?;
            if let Timed::OnTime(late) = timed {
                break late;
            }
            crate::console::println!(
                "  signalfd attempt {attempt} of {ATTEMPTS}: a waiter in {} came back {} ms \
                 after its signal, {}",
                how.call(),
                timed.late() / MILLI,
                timed.ended_by(),
            );
            if attempt == ATTEMPTS {
                return Err(timed.failure());
            }
            attempt += 1;
        };
        counts.late = counts.late.max(late);
        counts.signals += 1;
    }
    Ok(())
}

/// One attempt of [`check_waiters_are_woken`]: a new signalfd, a waiter on
/// it, the signal, and how the waiter came back. The signal is taken whatever
/// the timing, so the next attempt's waiter finds nothing pending.
fn time_a_waiter(process: &Arc<Process>, page: u64, how: HowWaits) -> Result<Timed, &'static str> {
    let made = create(process, page, bit(SIGUSR1), 0)?;
    let file = opened(process, made)?;
    let inner = signalfd::of(&file).ok_or("a signalfd is not one")?;
    let target = watch(process, page, made, how)?;
    *ANSWER.lock() = None;
    *WAITER.lock() = Some(Waiter {
        process: Arc::clone(process),
        page,
        target,
        file: Arc::clone(&file),
        how,
    });
    // An `epoll_wait` sleeps on its set's queue, which the signal's wake
    // wakes through the registration's hook (`fs::epoll`'s ready list).
    let set = match how {
        HowWaits::Epoll => Some(
            crate::fs::epoll::of(&*opened(process, target)?).ok_or("an epoll set is not one")?,
        ),
        HowWaits::Read | HowWaits::Poll => None,
    };
    let timed = wait_out_a_signal(process, &inner, how, set.as_deref())?;
    drop(set);
    if how != HowWaits::Read {
        // The signal the waiter was told of, still to be taken.
        let nonblocking = create(process, page, bit(SIGUSR1), SFD_NONBLOCK)?;
        let infos = read_infos(process, page, nonblocking, 1)
            .map_err(|_| "a signal a poll or epoll_wait was woken for could not be read")?;
        if !matches!(infos.as_slice(), [(SIGUSR1, ..)]) {
            return Err("a signal a poll or epoll_wait was woken for did not read back");
        }
        closed(process, nonblocking)?;
    }
    if how == HowWaits::Epoll {
        closed(process, target)?;
    }
    drop(inner);
    drop(file);
    closed(process, made)?;
    Ok(timed)
}

/// What the waiting task watches: the signalfd itself for a read or a `poll`,
/// with a `pollfd` staged for the latter, or a new epoll set holding it.
fn watch(process: &Process, page: u64, made: i32, how: HowWaits) -> Result<i32, &'static str> {
    match how {
        HowWaits::Read => Ok(made),
        HowWaits::Poll => {
            let mut entry = [0_u8; 8];
            for (slot, byte) in entry.iter_mut().zip(made.to_le_bytes()) {
                *slot = byte;
            }
            for (slot, byte) in entry.iter_mut().skip(4).zip(poll::POLLIN.to_le_bytes()) {
                *slot = byte;
            }
            uaccess::copy_to_user(process.space(), page + AT_EVENT, &entry)
                .map_err(|_| "could not stage a pollfd")?;
            Ok(made)
        }
        HowWaits::Epoll => {
            let set = descriptor(
                epoll::sys_epoll_create1(process, 0),
                "epoll_create1 was refused",
            )?;
            uaccess::copy_to_user(
                process.space(),
                page + AT_EVENT,
                &epoll::encode(EPOLLIN, 0x5F),
            )
            .map_err(|_| "could not stage an epoll event")?;
            if epoll::sys_epoll_ctl(process, set, EPOLL_CTL_ADD, made, page + AT_EVENT) != Ok(0) {
                return Err("a signalfd could not be added to an epoll set");
            }
            Ok(set)
        }
    }
}

/// Start the waiter, send the signal once it waits, and answer how late after
/// the send it came back, and whether the signal's wake is what ended its
/// wait.
fn wait_out_a_signal(
    process: &Process,
    inner: &SignalFd,
    how: HowWaits,
    set: Option<&crate::fs::epoll::Epoll>,
) -> Result<Timed, &'static str> {
    let ended = || {
        set.map_or_else(
            || inner.waits_ended_by_a_wake(),
            |set| set.queue().waits_ended_by_a_wake(),
        )
    };
    let ended_before = ended();
    let waiter = crate::sched::spawn("signalfd-waiter", waiter, 0, ferrix_sched::NICE_0_WEIGHT)?;
    until_listed(inner, set);
    if ANSWER.lock().is_some() {
        return Err("a read, poll or epoll_wait on a signalfd with nothing pending did not wait");
    }
    let sent = crate::timer::now_nanos();
    kill::send(
        process,
        SIGUSR1,
        Origin::User {
            pid: SENDER,
            uid: 0,
        },
    );
    let patience = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = DONE.wait_until_deadline(|| ANSWER.lock().is_some(), patience);
    let answer = ANSWER.lock().take();
    crate::sched::wait_until_gone(&waiter, crate::sched::REAPER_PATIENCE_NANOS)?;
    drop(waiter);
    let Some((answer, back)) = answer else {
        return Err("a read, poll or epoll_wait on a signalfd never came back after the signal");
    };
    let wanted = match how {
        HowWaits::Read => SIGUSR1 as usize,
        HowWaits::Poll | HowWaits::Epoll => 1,
    };
    if answer != Ok(wanted) {
        return Err("a read, poll or epoll_wait woken by a signal did not answer it");
    }
    let late = back.saturating_sub(sent);
    if ended() == ended_before {
        return Ok(Timed::Rechecked(late));
    }
    if late > LATE_LIMIT_NANOS {
        return Ok(Timed::Late(late));
    }
    Ok(Timed::OnTime(late))
}

/// Wait until a task is listed on the signals' queue, has answered, or the
/// patience runs out -- whichever is first. The check that follows says
/// which. Only then is the signal sent, so the waiter is waiting when it
/// arrives however long the host took to run it.
fn until_listed(inner: &SignalFd, set: Option<&crate::fs::epoll::Epoll>) {
    let deadline = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let listed = || set.map_or_else(|| inner.waiters_listed(), |set| set.queue().listed());
    while listed() == 0 && ANSWER.lock().is_none() && crate::timer::now_nanos() < deadline {
        crate::sched::sleep_for(LISTED_LOOK_NANOS);
    }
}

/// The waiting task: one blocking read, `poll` or `epoll_wait` on what
/// [`WAITER`] names, and the counter when it came back. A read answers the
/// signal's number.
fn waiter(_argument: usize) {
    let subject = WAITER.lock().take();
    let answer = match subject {
        Some(Waiter {
            file,
            how: HowWaits::Read,
            ..
        }) => {
            let mut info = [0_u8; SIGNALFD_SIGINFO_BYTES];
            file.read(&mut info).and_then(|got| {
                let signo = info
                    .first_chunk::<4>()
                    .map(|bytes| u32::from_le_bytes(*bytes));
                match (got, signo) {
                    (SIGNALFD_SIGINFO_BYTES, Some(signo)) => Ok(signo as usize),
                    _ => Err(Errno::EIO),
                }
            })
        }
        Some(Waiter {
            process,
            page,
            how: HowWaits::Poll,
            ..
        }) => poll::sys_poll(&process, page + AT_EVENT, 1, WAITER_TIMEOUT_MILLIS),
        Some(Waiter {
            process,
            page,
            target,
            how: HowWaits::Epoll,
            ..
        }) => epoll::sys_epoll_wait(&process, target, page + AT_EVENTS, 4, WAITER_TIMEOUT_MILLIS),
        None => Err(Errno::ESRCH),
    };
    let back = crate::timer::now_nanos();
    *ANSWER.lock() = Some((answer, back));
    DONE.wake_all();
}

/// Take every signal still pending for the process, through a signalfd of
/// every signal, and fail if one was.
fn drain(process: &Process, page: u64) -> Result<(), &'static str> {
    let all = create(process, page, u64::MAX, SFD_NONBLOCK)?;
    let mut left = 0;
    while let Ok(infos) = read_infos(process, page, all, 3) {
        left += infos.len();
    }
    closed(process, all)?;
    if left != 0 || pending(process) != 0 {
        return Err("a signal the signalfd check sent was still pending after it");
    }
    Ok(())
}

/// The signals pending for the process as a whole.
fn pending(process: &Process) -> u64 {
    process.with_signals(|signals| signals.pending())
}

/// Stage a signal mask where [`AT_MASK`] is.
fn stage_mask(process: &Process, page: u64, mask: u64) -> Result<(), &'static str> {
    uaccess::copy_to_user(process.space(), page + AT_MASK, &mask.to_le_bytes())
        .map_err(|_| "could not stage a signal mask")
}

/// `signalfd4(-1, mask, 8, flags)` by number, as a descriptor.
fn create(process: &Process, page: u64, mask: u64, flags: u32) -> Result<i32, &'static str> {
    stage_mask(process, page, mask)?;
    descriptor(
        by_number(
            process,
            Syscall::Signalfd4,
            [u64::MAX, page + AT_MASK, 8, u64::from(flags), 0, 0],
        ),
        "signalfd4 was refused",
    )
}

/// The open file a descriptor names.
fn opened(process: &Process, made: i32) -> Result<Arc<OpenFile>, &'static str> {
    fd::file(process, made).map_err(|_| "a signalfd the check made is gone")
}

/// The signalfd a descriptor names.
fn of(process: &Process, made: i32) -> Result<Arc<SignalFd>, &'static str> {
    let file = opened(process, made)?;
    signalfd::of(&file).ok_or("a signalfd is not one")
}

/// Read up to `slots` signals through descriptor `made` by number, and answer
/// each one's `ssi_signo`, `ssi_code` and `ssi_pid`.
fn read_infos(
    process: &Process,
    page: u64,
    made: i32,
    slots: u64,
) -> Result<alloc::vec::Vec<(u32, i32, u32)>, Errno> {
    let size = SIGNALFD_SIGINFO_BYTES as u64;
    let got = by_number(
        process,
        Syscall::Read,
        [
            u64::from(made.cast_unsigned()),
            page + AT_INFO,
            slots * size,
            0,
            0,
            0,
        ],
    )?;
    if got % SIGNALFD_SIGINFO_BYTES != 0 || got == 0 {
        return Err(Errno::EIO);
    }
    let mut infos = alloc::vec::Vec::new();
    for at in (0..got as u64).step_by(SIGNALFD_SIGINFO_BYTES) {
        let mut info = [0_u8; SIGNALFD_SIGINFO_BYTES];
        uaccess::copy_from_user(process.space(), page + AT_INFO + at, &mut info)
            .map_err(|_| Errno::EFAULT)?;
        let word = |offset: usize| {
            info.get(offset..offset + 4)
                .and_then(|bytes| bytes.try_into().ok())
                .map_or(0, u32::from_le_bytes)
        };
        infos.push((word(0), word(8) as i32, word(12)));
    }
    Ok(infos)
}

/// Close a descriptor the check opened.
fn closed(process: &Process, made: i32) -> Result<(), &'static str> {
    fd::sys_close(process, made)
        .map(|_| ())
        .map_err(|_| "a descriptor the signalfd check opened would not close")
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
