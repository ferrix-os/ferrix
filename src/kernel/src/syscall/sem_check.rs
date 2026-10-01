//! System V semaphores, proved at boot: `semget`, `semop`, `semtimedop`,
//! `semctl` and `SEM_UNDO` at exit, through the functions the system calls
//! reach (`syscall::sem`), and the heap each kind of record takes charged to
//! a job and given back on every way out (F-37).
//!
//! The blocking cases run a waiter task of the check's own, as the futex
//! checks do: it is woken by a `semop`, by `SETVAL`, by `IPC_RMID` with
//! `EIDRM`, by an interruption with `EINTR`, and by its deadline with
//! `EAGAIN`, and each time the job it ran in must read zero heap afterwards.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use ferrix_linux_abi::errno::Errno;

use crate::object::job::Job;
use crate::object::quota::Resource;
use crate::sched::{self, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::process;
use crate::syscall::sem::{
    self, Caller, Interrupt, Layout, Memory, SEMOPM, SEMVMX, SemBuf, UndoList, flags,
};

/// What the check saw, for the boot line.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Calls answered as Linux answers them.
    pub(crate) calls: usize,
    /// Of them, refusals with Linux's error.
    pub(crate) refusals: usize,
    /// Blocked callers ended, each by one of the five ways.
    pub(crate) waits: usize,
    /// Sets a job made before the per-job bound (at the check's bound)
    /// refused one, while a sibling made one.
    pub(crate) per_job: usize,
}

/// `IPC_RMID` and the other commands, as `semctl` takes them.
mod cmd {
    /// Remove.
    pub(super) const RMID: i32 = 0;
    /// Set owner and mode.
    pub(super) const SET: i32 = 1;
    /// Read the `semid64_ds`.
    pub(super) const STAT: i32 = 2;
    /// Read the limits.
    pub(super) const INFO: i32 = 3;
    /// Last operator.
    pub(super) const GETPID: i32 = 11;
    /// A value.
    pub(super) const GETVAL: i32 = 12;
    /// Every value.
    pub(super) const GETALL: i32 = 13;
    /// Callers waiting to decrement.
    pub(super) const GETNCNT: i32 = 14;
    /// Callers waiting for zero.
    pub(super) const GETZCNT: i32 = 15;
    /// Set a value.
    pub(super) const SETVAL: i32 = 16;
    /// Set every value.
    pub(super) const SETALL: i32 = 17;
    /// The layout flag.
    pub(super) const IPC_64: i32 = 0x100;
}

/// The per-job bound the check fills to, standing in for
/// [`sem::SETS_PER_JOB`].
const PER_JOB: usize = 6;

/// How long the check waits for a waiter to wait, or to come back.
const PATIENCE_NANOS: u64 = 5_000_000_000;

/// Memory of the check's own for `semctl` to read and write: the address is
/// an offset into it.
#[derive(Debug)]
struct Buffer(SpinLock<Vec<u8>>);

impl Buffer {
    /// 256 zero bytes.
    fn new() -> Buffer {
        Buffer(SpinLock::new(vec![0; 256]))
    }

    /// The little-endian `u32` at `at`.
    fn u32_at(&self, at: usize) -> u32 {
        let bytes = self.0.lock();
        bytes
            .get(at..at + 4)
            .and_then(|four| four.try_into().ok())
            .map_or(0, u32::from_le_bytes)
    }
}

impl Memory for Buffer {
    fn write(&self, at: u64, bytes: &[u8]) -> Result<(), Errno> {
        let at = usize::try_from(at).map_err(|_| Errno::EFAULT)?;
        let mut held = self.0.lock();
        let to = held
            .get_mut(at..at.saturating_add(bytes.len()))
            .ok_or(Errno::EFAULT)?;
        to.copy_from_slice(bytes);
        Ok(())
    }

    fn read(&self, at: u64, bytes: &mut [u8]) -> Result<(), Errno> {
        let at = usize::try_from(at).map_err(|_| Errno::EFAULT)?;
        let held = self.0.lock();
        let from = held
            .get(at..at.saturating_add(bytes.len()))
            .ok_or(Errno::EFAULT)?;
        bytes.copy_from_slice(from);
        Ok(())
    }
}

/// Root, as the check's caller.
fn root() -> Caller {
    Caller {
        pid: 4242,
        uid: 0,
        gid: 0,
        groups: Vec::new(),
        privileged: true,
        ns: Arc::clone(sem::initial_ipc()),
    }
}

/// A caller of uid and gid 1000, and nothing more.
fn stranger() -> Caller {
    Caller {
        pid: 4343,
        uid: 1000,
        gid: 1000,
        groups: Vec::new(),
        privileged: false,
        ns: Arc::clone(sem::initial_ipc()),
    }
}

/// A set the check holds, removed as it is dropped.
#[derive(Debug)]
pub(crate) struct Held(pub(crate) i32);

impl Drop for Held {
    fn drop(&mut self) {
        let _ = sem::semctl(
            &root(),
            &Buffer::new(),
            Layout::Narrow,
            self.0,
            0,
            cmd::RMID,
            0,
        );
    }
}

/// Make a private set of `nsems` with `mode`, as root would.
pub(crate) fn private_set(nsems: i32, mode: i32) -> Result<Held, Errno> {
    sem::semget(&root(), 0, nsems, flags::CREAT | mode).map(Held)
}

/// Never interrupted.
#[derive(Debug)]
struct Never;

impl Interrupt for Never {
    fn interrupted(&self) -> bool {
        false
    }
}

/// Interrupted once [`INTERRUPT`] is set.
#[derive(Debug)]
struct Flagged;

/// Set to interrupt the waiter.
static INTERRUPT: AtomicBool = AtomicBool::new(false);

impl Interrupt for Flagged {
    fn interrupted(&self) -> bool {
        INTERRUPT.load(Ordering::Acquire)
    }
}

/// What the waiter task is to do.
#[derive(Debug)]
struct Subject {
    /// The set.
    id: i32,
    /// Its operations.
    sops: Vec<SemBuf>,
    /// Its deadline on the counter.
    deadline: Option<u64>,
    /// The job it runs in.
    group: u32,
    /// Its process's undo list.
    undo: Arc<UndoList>,
}

/// The waiter's task's orders.
static SUBJECT: SpinLock<Option<Subject>> = SpinLock::new(None);
/// The waiter's answer.
static ANSWER: SpinLock<Option<Result<usize, Errno>>> = SpinLock::new(None);
/// Woken as the waiter answers.
static ANSWERED: WaitQueue = WaitQueue::new();

/// The waiter task: one `semop` on what [`SUBJECT`] names, in its job.
fn waiter(_argument: usize) {
    let Some(subject) = SUBJECT.lock().take() else {
        *ANSWER.lock() = Some(Err(Errno::ESRCH));
        ANSWERED.wake_all();
        return;
    };
    let own = sched::running_group();
    sched::set_current_group(subject.group);
    let answer = sem::semop(
        &root(),
        &subject.undo,
        subject.id,
        subject.sops,
        subject.deadline,
        &Flagged,
    );
    // What its process owes is paid, as its end would pay it, still in its
    // job, so the undo record's charge comes back with it.
    sem::exit(&subject.undo, 4242);
    sched::set_current_group(own);
    drop(subject.undo);
    *ANSWER.lock() = Some(answer);
    ANSWERED.wake_all();
}

/// Start the waiter on `sops` against set `id` in `job`, and wait until
/// `semctl(query)` of semaphore 0 reads one waiter.
fn start_waiter(
    id: i32,
    sops: Vec<SemBuf>,
    deadline: Option<u64>,
    job: &Job,
    query: i32,
) -> Result<Arc<sched::Task>, &'static str> {
    INTERRUPT.store(false, Ordering::Release);
    *ANSWER.lock() = None;
    *SUBJECT.lock() = Some(Subject {
        id,
        sops,
        deadline,
        group: job.quota_index(),
        undo: Arc::new(UndoList::new()),
    });
    let task = sched::spawn("sem-waiter", waiter, 0, ferrix_sched::NICE_0_WEIGHT)?;
    let until = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    while ctl(id, 0, query, 0) != Ok(1) {
        if ANSWER.lock().is_some() {
            return Err("sem: a waiter returned without waiting");
        }
        if crate::timer::now_nanos() >= until {
            return Err("sem: a waiter never started waiting");
        }
        sched::sleep_for(1_000_000);
    }
    Ok(task)
}

/// Wait for the waiter's answer.
fn waiter_answer() -> Result<Result<usize, Errno>, &'static str> {
    let until = crate::timer::now_nanos().saturating_add(PATIENCE_NANOS);
    let _ = ANSWERED.wait_until_deadline(|| ANSWER.lock().is_some(), until);
    ANSWER.lock().take().ok_or("sem: a waiter never came back")
}

/// `semctl` as root with a scratch buffer.
fn ctl(id: i32, num: i32, command: i32, arg: u64) -> Result<usize, Errno> {
    sem::semctl(
        &root(),
        &Buffer::new(),
        Layout::Narrow,
        id,
        num,
        command,
        arg,
    )
}

/// `semop` as root, never waiting past `deadline`.
fn op(id: i32, sops: &[SemBuf], undo: &UndoList) -> Result<usize, Errno> {
    sem::semop(&root(), undo, id, sops.to_vec(), Some(0), &Never)
}

/// Count `answer` against `expected`, a refusal if it is an error.
fn expect<T: PartialEq + Copy>(
    report: &mut Report,
    answer: Result<T, Errno>,
    expected: Result<T, Errno>,
    what: &'static str,
) -> Result<(), &'static str> {
    if answer != expected {
        crate::console::println!("  sem      {what}");
        return Err(what);
    }
    report.calls += 1;
    if expected.is_err() {
        report.refusals += 1;
    }
    Ok(())
}

/// Run every check.
///
/// # Errors
///
/// Which property failed.
///
/// Verifies: H.QUOTA.7
pub(crate) fn run() -> Result<Report, &'static str> {
    let before = sem::sets_in_use();
    let mut report = Report::default();
    keys(&mut report)?;
    values(&mut report)?;
    operations(&mut report)?;
    layouts(&mut report)?;
    undo_at_exit(&mut report)?;
    {
        let tree = Job::new_root().map_err(|_| "sem: no memory for a job")?;
        waits(&mut report, &tree)?;
        reopened(&mut report, &tree)?;
        report.per_job = per_job(&tree)?;
    }
    if sem::sets_in_use() != before {
        return Err("sem: the check's sets were not all removed");
    }
    Ok(report)
}

/// `semget` by key: made, found, refused as Linux refuses.
fn keys(report: &mut Report) -> Result<(), &'static str> {
    const KEY: i32 = 0x5e3a_0001;
    let made = sem::semget(&root(), KEY, 2, flags::CREAT | flags::EXCL | 0o600)
        .map_err(|_| "sem: a keyed set was not made")?;
    let held = Held(made);
    let r = &mut *report;
    expect(
        r,
        sem::semget(&root(), KEY, 2, flags::CREAT | flags::EXCL | 0o600),
        Err(Errno::EEXIST),
        "sem: IPC_EXCL on a used key was not EEXIST",
    )?;
    expect(
        r,
        sem::semget(&root(), KEY, 1, 0o600),
        Ok(made),
        "sem: a key did not find its set",
    )?;
    expect(
        r,
        sem::semget(&root(), KEY, 3, 0),
        Err(Errno::EINVAL),
        "sem: more semaphores than the set has was not EINVAL",
    )?;
    expect(
        r,
        sem::semget(&root(), KEY + 1, 1, 0),
        Err(Errno::ENOENT),
        "sem: a key with no set and no IPC_CREAT was not ENOENT",
    )?;
    expect(
        r,
        sem::semget(&root(), 0, 0, flags::CREAT),
        Err(Errno::EINVAL),
        "sem: a set of none was not EINVAL",
    )?;
    expect(
        r,
        sem::semget(&root(), 0, 32_001, flags::CREAT),
        Err(Errno::EINVAL),
        "sem: a set past SEMMSL was not EINVAL",
    )?;
    expect(
        r,
        sem::semget(&stranger(), KEY, 1, 0o600),
        Err(Errno::EACCES),
        "sem: a stranger found a set of mode 0600",
    )?;
    expect(
        r,
        sem::semctl(
            &stranger(),
            &Buffer::new(),
            Layout::Narrow,
            made,
            0,
            cmd::GETVAL,
            0,
        ),
        Err(Errno::EACCES),
        "sem: a stranger read a value of mode 0600",
    )?;
    expect(
        r,
        sem::semctl(
            &stranger(),
            &Buffer::new(),
            Layout::Narrow,
            made,
            0,
            cmd::RMID,
            0,
        ),
        Err(Errno::EPERM),
        "sem: a stranger removed a set it does not own",
    )?;
    drop(held);
    expect(
        r,
        ctl(made, 0, cmd::GETVAL, 0),
        Err(Errno::EINVAL),
        "sem: a removed set's id still answered",
    )?;
    Ok(())
}

/// `SETVAL`, `GETVAL`, `SETALL`, `GETALL` and `GETPID`.
fn values(report: &mut Report) -> Result<(), &'static str> {
    let set = private_set(3, 0o600).map_err(|_| "sem: no set for the values")?;
    let id = set.0;
    let r = &mut *report;
    expect(r, ctl(id, 1, cmd::SETVAL, 7), Ok(0), "sem: SETVAL failed")?;
    expect(
        r,
        ctl(id, 1, cmd::GETVAL, 0),
        Ok(7),
        "sem: GETVAL did not read SETVAL's value",
    )?;
    expect(
        r,
        ctl(id, 1, cmd::GETPID, 0),
        Ok(4242),
        "sem: GETPID did not name SETVAL's caller",
    )?;
    expect(
        r,
        ctl(id, 3, cmd::GETVAL, 0),
        Err(Errno::EINVAL),
        "sem: a semaphore past the set was not EINVAL",
    )?;
    expect(
        r,
        ctl(id, 0, cmd::SETVAL, 32_768),
        Err(Errno::ERANGE),
        "sem: a value past SEMVMX was not ERANGE",
    )?;
    let buffer = Buffer::new();
    buffer
        .write(0, &[1, 0, 2, 0, 3, 0])
        .map_err(|_| "sem: buffer")?;
    expect(
        r,
        sem::semctl(&root(), &buffer, Layout::Narrow, id, 0, cmd::SETALL, 0),
        Ok(0),
        "sem: SETALL failed",
    )?;
    expect(
        r,
        sem::semctl(&root(), &buffer, Layout::Narrow, id, 0, cmd::GETALL, 16),
        Ok(0),
        "sem: GETALL failed",
    )?;
    let all = [buffer.u32_at(16), buffer.u32_at(20) & 0xFFFF];
    if all != [0x0002_0001, 3] || ctl(id, 2, cmd::GETVAL, 0) != Ok(3) {
        return Err("sem: GETALL did not read what SETALL wrote");
    }
    expect(
        r,
        sem::semctl(&root(), &buffer, Layout::Narrow, id, 0, 99, 0),
        Err(Errno::EINVAL),
        "sem: an unknown command was not EINVAL",
    )?;
    Ok(())
}

/// `semop`: all or nothing, `IPC_NOWAIT`, the limits.
fn operations(report: &mut Report) -> Result<(), &'static str> {
    let set = private_set(2, 0o600).map_err(|_| "sem: no set for the operations")?;
    let id = set.0;
    let undo = UndoList::new();
    let r = &mut *report;
    let nowait = flags::NOWAIT;
    expect(
        r,
        op(id, &[SemBuf::new(0, -1, nowait)], &undo),
        Err(Errno::EAGAIN),
        "sem: a decrement below zero with IPC_NOWAIT was not EAGAIN",
    )?;
    // The first would go, the second cannot: neither is done.
    expect(
        r,
        op(
            id,
            &[SemBuf::new(0, 1, 0), SemBuf::new(1, -1, nowait)],
            &undo,
        ),
        Err(Errno::EAGAIN),
        "sem: a call half of which could go was not EAGAIN",
    )?;
    expect(
        r,
        ctl(id, 0, cmd::GETVAL, 0),
        Ok(0),
        "sem: a refused call left one of its operations done",
    )?;
    expect(
        r,
        op(
            id,
            &[
                SemBuf::new(0, 2, 0),
                SemBuf::new(0, -1, 0),
                SemBuf::new(1, 0, 0),
            ],
            &undo,
        ),
        Ok(0),
        "sem: operations that could all go failed",
    )?;
    expect(
        r,
        ctl(id, 0, cmd::GETVAL, 0),
        Ok(1),
        "sem: two operations on one semaphore did not add up",
    )?;
    expect(
        r,
        op(id, &[SemBuf::new(0, 0, nowait)], &undo),
        Err(Errno::EAGAIN),
        "sem: a wait for zero on one with IPC_NOWAIT was not EAGAIN",
    )?;
    expect(
        r,
        op(
            id,
            &[SemBuf::new(1, SEMVMX as i16, 0), SemBuf::new(1, 1, 0)],
            &undo,
        ),
        Err(Errno::ERANGE),
        "sem: a value past SEMVMX was not ERANGE",
    )?;
    expect(
        r,
        op(id, &[SemBuf::new(2, 1, 0)], &undo),
        Err(Errno::EFBIG),
        "sem: a semaphore past the set was not EFBIG",
    )?;
    expect(
        r,
        op(id + 1, &[SemBuf::new(0, 1, 0)], &undo),
        Err(Errno::EINVAL),
        "sem: an id with no set was not EINVAL",
    )?;
    expect(
        r,
        sem::semop(
            &stranger(),
            &undo,
            id,
            vec![SemBuf::new(0, 1, 0)],
            Some(0),
            &Never,
        ),
        Err(Errno::EACCES),
        "sem: a stranger altered a set of mode 0600",
    )?;
    // A deadline already passed on one that cannot go: EAGAIN, as a
    // timeout is.
    expect(
        r,
        op(id, &[SemBuf::new(1, -1, 0)], &undo),
        Err(Errno::EAGAIN),
        "sem: a passed deadline was not EAGAIN",
    )?;
    Ok(())
}

/// `IPC_STAT`, `IPC_SET` and `IPC_INFO` in the layouts, against the offsets
/// the UAPI headers give, compiled for x86-64 and ARM.
fn layouts(report: &mut Report) -> Result<(), &'static str> {
    let set = private_set(5, 0o640).map_err(|_| "sem: no set for the layouts")?;
    let id = set.0;
    let r = &mut *report;
    for (layout, nsems_at) in [
        (Layout::Narrow, 52),
        (Layout::X86_64, 80),
        (Layout::Generic64, 64),
    ] {
        let buffer = Buffer::new();
        let command = cmd::STAT | cmd::IPC_64;
        expect(
            r,
            sem::semctl(&root(), &buffer, layout, id, 0, command, 0),
            Ok(0),
            "sem: IPC_STAT failed",
        )?;
        if buffer.u32_at(20) != 0o640 || buffer.u32_at(nsems_at) != 5 || buffer.u32_at(0) != 0 {
            return Err("sem: IPC_STAT's key, mode or sem_nsems was not where the header puts it");
        }
    }
    expect(
        r,
        sem::semctl(&root(), &Buffer::new(), Layout::Narrow, id, 0, cmd::STAT, 0),
        Err(Errno::EINVAL),
        "sem: a 32-bit IPC_STAT without IPC_64 was not EINVAL",
    )?;
    let buffer = Buffer::new();
    buffer
        .write(4, &1000_u32.to_le_bytes())
        .map_err(|_| "sem: buffer")?;
    buffer
        .write(8, &1000_u32.to_le_bytes())
        .map_err(|_| "sem: buffer")?;
    buffer
        .write(20, &0o600_u32.to_le_bytes())
        .map_err(|_| "sem: buffer")?;
    expect(
        r,
        sem::semctl(&root(), &buffer, Layout::X86_64, id, 0, cmd::SET, 0),
        Ok(0),
        "sem: IPC_SET failed",
    )?;
    // The stranger now owns it, and may read it.
    let stranger_reads = sem::semctl(
        &stranger(),
        &Buffer::new(),
        Layout::X86_64,
        id,
        0,
        cmd::GETVAL,
        0,
    );
    expect(
        r,
        stranger_reads,
        Ok(0),
        "sem: IPC_SET did not hand the set over",
    )?;
    let info = Buffer::new();
    let answered = sem::semctl(&root(), &info, Layout::Narrow, 0, 0, cmd::INFO, 0);
    if answered.is_err()
        || info.u32_at(16) != 32_000
        || info.u32_at(20) != SEMOPM as u32
        || info.u32_at(32) != SEMVMX as u32
    {
        return Err("sem: IPC_INFO's semmsl, semopm or semvmx was wrong");
    }
    report.calls += 1;
    Ok(())
}

/// A keyed set made in one job by a process that held it with `SEM_UNDO`
/// and has ended, found by its key and used from another job by a new
/// process, as the Steam client finds its mutex when it restarts: `EEXIST`
/// to `IPC_EXCL`, the set to a plain `semget`, the value its undo left,
/// the ended process as its last operator, and the set its to remove and
/// make again.
fn reopened(report: &mut Report, tree: &Arc<Job>) -> Result<(), &'static str> {
    const KEY: i32 = 0x5e3a_813e;
    let first_job = tree.new_child().map_err(|_| "sem: a job refused a child")?;
    let second_job = tree.new_child().map_err(|_| "sem: a job refused a child")?;
    let first = process::new_for_check().map_err(|_| "sem: no process for the reopen")?;
    let caller = Caller::of(&first);
    let made = as_task_of(&first_job, || {
        let id = sem::semget(&caller, KEY, 1, flags::CREAT | flags::EXCL | 0o600)?;
        let _ = ctl(id, 0, cmd::SETVAL, 1);
        let taken = vec![SemBuf::new(0, -1, flags::UNDO)];
        sem::semop(&caller, first.sem_undo(), id, taken, None, &Never).map(|_| id)
    })
    .map_err(|_| "sem: the first process could not make and take its set")?;
    let held = Held(made);
    let first_pid = first.pid();
    process::kill(&first, 0);
    drop(first);
    let second = process::new_for_check().map_err(|_| "sem: no process for the reopen")?;
    let caller = Caller::of(&second);
    let r = &mut *report;
    as_task_of(&second_job, || -> Result<(), &'static str> {
        let again = sem::semget(&caller, KEY, 1, flags::CREAT | flags::EXCL | 0o600);
        expect(
            r,
            again,
            Err(Errno::EEXIST),
            "sem: IPC_EXCL on an ended process's key was not EEXIST",
        )?;
        expect(
            r,
            sem::semget(&caller, KEY, 1, 0o600),
            Ok(made),
            "sem: a new process in another job did not find the set by its key",
        )?;
        let read = |command| {
            sem::semctl(
                &caller,
                &Buffer::new(),
                Layout::Narrow,
                made,
                0,
                command | cmd::IPC_64,
                0,
            )
        };
        expect(
            r,
            read(cmd::GETVAL),
            Ok(1),
            "sem: the ended process's SEM_UNDO was not paid",
        )?;
        expect(
            r,
            read(cmd::GETNCNT),
            Ok(0),
            "sem: GETNCNT counted a waiter nobody is",
        )?;
        expect(
            r,
            read(cmd::GETPID),
            Ok(first_pid as usize),
            "sem: GETPID did not name the ended process",
        )?;
        let mine = vec![SemBuf::new(0, -1, flags::UNDO)];
        expect(
            r,
            sem::semop(&caller, second.sem_undo(), made, mine, Some(0), &Never),
            Ok(0),
            "sem: a new process could not take the set",
        )?;
        Ok(())
    })?;
    process::kill(&second, 0);
    drop(second);
    drop(held);
    for job in [&first_job, &second_job] {
        if job.usage(Resource::Kernel).map_or(0, |usage| usage.used) != 0 {
            return Err("sem: a removed set's heap, or an undo record's, was still charged");
        }
    }
    Ok(())
}

/// `SEM_UNDO`: a process that took one and ended gives it back, and is the
/// semaphore's last operator.
fn undo_at_exit(report: &mut Report) -> Result<(), &'static str> {
    let set = private_set(1, 0o600).map_err(|_| "sem: no set for the undo")?;
    let id = set.0;
    let r = &mut *report;
    expect(r, ctl(id, 0, cmd::SETVAL, 1), Ok(0), "sem: SETVAL failed")?;
    let process = process::new_for_check().map_err(|_| "sem: no process for the undo")?;
    let caller = Caller::of(&process);
    let taken = sem::semop(
        &caller,
        process.sem_undo(),
        id,
        vec![SemBuf::new(0, -1, flags::UNDO)],
        None,
        &Never,
    );
    expect(r, taken, Ok(0), "sem: a SEM_UNDO decrement failed")?;
    expect(
        r,
        ctl(id, 0, cmd::GETVAL, 0),
        Ok(0),
        "sem: a SEM_UNDO decrement did not decrement",
    )?;
    process::kill(&process, 0);
    expect(
        r,
        ctl(id, 0, cmd::GETVAL, 0),
        Ok(1),
        "sem: a process's end did not undo its SEM_UNDO decrement",
    )?;
    expect(
        r,
        ctl(id, 0, cmd::GETPID, 0),
        Ok(process.pid() as usize),
        "sem: the undo at exit did not record the process as last operator",
    )?;
    drop(process);
    Ok(())
}

/// Every way a blocked `semop` ends, each in `tree`'s child job, which must
/// hold no heap once the waiter is gone.
fn waits(report: &mut Report, tree: &Arc<Job>) -> Result<(), &'static str> {
    let job = tree.new_child().map_err(|_| "sem: a job refused a child")?;
    let set = private_set(1, 0o600).map_err(|_| "sem: no set for the waits")?;
    let id = set.0;
    // Woken by a `semop` that lets it through.
    let task = start_waiter(
        id,
        vec![SemBuf::new(0, -1, flags::UNDO)],
        None,
        &job,
        cmd::GETNCNT,
    )?;
    let undo = UndoList::new();
    let _ = op(id, &[SemBuf::new(0, 1, 0)], &undo);
    ended(report, &job, waiter_answer()?, Ok(0), "a semop's increment")?;
    drop(task);
    // Waiting for zero, woken by SETVAL.
    let _ = ctl(id, 0, cmd::SETVAL, 1);
    let task = start_waiter(id, vec![SemBuf::new(0, 0, 0)], None, &job, cmd::GETZCNT)?;
    let _ = ctl(id, 0, cmd::SETVAL, 0);
    ended(report, &job, waiter_answer()?, Ok(0), "SETVAL to zero")?;
    drop(task);
    // Interrupted.
    let task = start_waiter(id, vec![SemBuf::new(0, -1, 0)], None, &job, cmd::GETNCNT)?;
    INTERRUPT.store(true, Ordering::Release);
    sched::wake(&task);
    ended(
        report,
        &job,
        waiter_answer()?,
        Err(Errno::EINTR),
        "an interruption",
    )?;
    drop(task);
    if ctl(id, 0, cmd::GETNCNT, 0) != Ok(0) {
        return Err("sem: an interrupted waiter was still counted");
    }
    // Its deadline.
    let deadline = crate::timer::now_nanos().saturating_add(20_000_000);
    let task = start_waiter(
        id,
        vec![SemBuf::new(0, -1, 0)],
        Some(deadline),
        &job,
        cmd::GETNCNT,
    )?;
    ended(
        report,
        &job,
        waiter_answer()?,
        Err(Errno::EAGAIN),
        "its deadline",
    )?;
    if crate::timer::now_nanos() < deadline {
        return Err("sem: a semtimedop timed out before its deadline");
    }
    drop(task);
    // Its set removed.
    let task = start_waiter(id, vec![SemBuf::new(0, -1, 0)], None, &job, cmd::GETNCNT)?;
    drop(set);
    ended(
        report,
        &job,
        waiter_answer()?,
        Err(Errno::EIDRM),
        "IPC_RMID",
    )?;
    drop(task);
    Ok(())
}

/// Check a waiter's answer, and that `job` holds no heap once it is gone.
fn ended(
    report: &mut Report,
    job: &Job,
    answer: Result<usize, Errno>,
    expected: Result<usize, Errno>,
    how: &'static str,
) -> Result<(), &'static str> {
    if answer != expected {
        crate::console::println!("  sem      a waiter ended by {how} answered {answer:?}");
        return Err("sem: a blocked semop was not answered as Linux answers it");
    }
    let held = job.usage(Resource::Kernel).map_or(0, |usage| usage.used);
    if held != 0 {
        crate::console::println!("  sem      a waiter ended by {how} left {held} bytes charged");
        return Err("sem: a waiter's record was not given back with its charge");
    }
    report.waits += 1;
    report.calls += 1;
    Ok(())
}

/// A job makes sets to the per-job bound and is refused `ENOSPC`, while a
/// sibling makes one; the heap each set took is its maker's, and comes back.
///
/// Verifies: H.QUOTA.7
fn per_job(tree: &Arc<Job>) -> Result<usize, &'static str> {
    let job = tree.new_child().map_err(|_| "sem: a job refused a child")?;
    let sibling = tree.new_child().map_err(|_| "sem: a job refused a child")?;
    let make = || sem::semget_capped(&root(), 0, 1, flags::CREAT | 0o600, PER_JOB).map(Held);
    let mut held = Vec::new();
    let refused = as_task_of(&job, || {
        loop {
            match make() {
                Ok(one) => held.push(one),
                Err(error) => return error,
            }
            if held.len() > PER_JOB {
                return Errno::E2BIG;
            }
        }
    });
    if refused != Errno::ENOSPC || held.len() != PER_JOB {
        return Err("sem: a job's sets were not bounded with ENOSPC");
    }
    let other = as_task_of(&sibling, make);
    let charged = job.usage(Resource::Kernel).map_or(0, |usage| usage.used);
    if other.is_err() || charged == 0 {
        return Err("sem: a job at its bound held back its sibling, or its sets were not charged");
    }
    let made = held.len();
    drop(held);
    drop(other);
    for one in [&job, &sibling] {
        if one.usage(Resource::Kernel).map_or(0, |usage| usage.used) != 0 {
            return Err("sem: removed sets' heap was still charged to their job");
        }
    }
    Ok(made)
}

/// Run `work` charged to `job`, as a task of it would be.
fn as_task_of<T>(job: &Job, work: impl FnOnce() -> T) -> T {
    let own = sched::running_group();
    sched::set_current_group(job.quota_index());
    let done = work();
    sched::set_current_group(own);
    done
}
