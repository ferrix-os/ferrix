//! What a process has said about itself: its name, its nice value, its I/O
//! priority, its personality, its robust futex list, and the switches `prctl`
//! flips. The nice value is acted on; the rest are kept and read back.
//!
//! # Why the ones that are not acted on are kept
//!
//! Because a program that sets one reads it back, and a program that reads
//! back something other than what it set concludes that the call failed.
//! `renice` prints the new value it reads with `getpriority`; `ionice` with no
//! command prints what `ioprio_get` says; `setarch` checks the persona it asked
//! for took. Answering `ENOSYS` makes every one of those applets fail, and
//! answering "accepted" while reading back the default makes them lie. So the
//! values are stored, reported, and -- until a block layer has a use for an
//! I/O class -- nothing else.
//!
//! # What the nice value does
//!
//! It is the scheduler's weight, through [`apply_nice`]: `ferrix_sched`'s
//! fair class has carried weights since stage 5 and `weight_of_nice` is
//! Linux's own table, so a nice value has somewhere to go. Until it was
//! plumbed through, nothing a program could say made the machine prefer one
//! of its programs to another -- a video decoder and the compositor drawing
//! its frames competed on equal terms, and the compositor lost as often as it
//! won. Namespaces and cgroups are stage 13 (`docs/ROADMAP.md`); a nice value
//! is what there is before them.
//!
//! # Why they are not fields of `Process`
//!
//! `Process` is changing under another piece of work, and these are the least
//! structural thing a process has. So they live here, in one table keyed by
//! pid, and move into the process when there is a reason to. Two consequences
//! follow, and both are stated rather than hidden:
//!
//! * **An entry is keyed by pid *and* start time.** Pids are reused, and a new
//!   process that happened to get a dead one's number must not inherit its
//!   nice value. A lookup whose start time does not match is a fresh process.
//!   Entries for processes that have gone are pruned when a new one is added,
//!   so the table is as long as the number of processes that have changed
//!   something, not the number that ever lived.
//! * **Nothing is inherited across `fork`.** Linux copies the nice value, the
//!   limits and the personality into a child; this table cannot see a child
//!   being made. A child starts from the defaults.
//!
//! The name `PR_SET_NAME` stores also survives `execve`, which on Linux resets
//! it to the new program's file name, because `execve` keeps the pid and the
//! start time this table keys on.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Deref;

use crate::sync::SpinLock;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_sched::weight_of_nice;

use crate::sched::Task;
use crate::syscall::credentials::{self, CAP_LAST_CAP};
use crate::syscall::pidns;
use crate::syscall::process::Process;
use crate::syscall::registry;
use crate::syscall::uaccess;

/// Bytes in a task's name, the terminator included: Linux's `TASK_COMM_LEN`.
pub(crate) const TASK_COMM_LEN: usize = 16;

/// How many resources `getrlimit` knows: `RLIM_NLIMITS` in
/// `asm-generic/resource.h`, 16 on all three architectures.
pub(crate) const RLIM_NLIMITS: usize = 16;

/// One resource limit, as the 64-bit `struct rlimit64` holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Limit {
    /// What the process is held to.
    pub(crate) soft: u64,
    /// How far it may raise `soft`.
    pub(crate) hard: u64,
}

/// Everything this module keeps for one process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attributes {
    /// `PR_SET_NAME`'s name, NUL-padded. `None` until it is set, which reads
    /// as the program's file name, as Linux's `comm` does.
    pub(crate) name: Option<[u8; TASK_COMM_LEN]>,
    /// `PR_SET_PDEATHSIG`: the signal asked for when the parent dies.
    pub(crate) parent_death_signal: u32,
    /// `PR_SET_DUMPABLE`. On by default, as it is on Linux.
    pub(crate) dumpable: bool,
    /// `PR_SET_NO_NEW_PRIVS`, which can be set and never cleared.
    pub(crate) no_new_privs: bool,
    /// `PR_SET_CHILD_SUBREAPER`.
    pub(crate) child_subreaper: bool,
    /// The nice value, -20 to 19.
    pub(crate) nice: i32,
    /// `ioprio_set`'s value, if one was set.
    pub(crate) io_priority: Option<u32>,
    /// The execution domain `personality` reports.
    pub(crate) personality: u32,
    /// Limits that were set, by resource. `None` reads as the default.
    pub(crate) limits: [Option<Limit>; RLIM_NLIMITS],
}

impl Default for Attributes {
    fn default() -> Self {
        Attributes {
            name: None,
            parent_death_signal: 0,
            dumpable: true,
            no_new_privs: false,
            child_subreaper: false,
            nice: 0,
            io_priority: None,
            personality: 0,
            limits: [None; RLIM_NLIMITS],
        }
    }
}

/// The table: pid to (start time, attributes).
static TABLE: SpinLock<BTreeMap<u32, (u64, Attributes)>> = SpinLock::new(BTreeMap::new());

/// What `process` has set, or the defaults.
pub(crate) fn get(process: &Process) -> Attributes {
    match TABLE.lock().get(&process.pid()) {
        Some((started, attributes)) if *started == process.started() => *attributes,
        _ => Attributes::default(),
    }
}

/// Change what is kept for `process`, and hand back what `change` returns.
pub(crate) fn update<R>(process: &Process, change: impl FnOnce(&mut Attributes) -> R) -> R {
    let (pid, started) = (process.pid(), process.started());
    let known = matches!(TABLE.lock().get(&pid), Some((at, _)) if *at == started);
    if !known {
        prune();
    }
    let mut table = TABLE.lock();
    let entry = table
        .entry(pid)
        .or_insert_with(|| (started, Attributes::default()));
    if entry.0 != started {
        *entry = (started, Attributes::default());
    }
    change(&mut entry.1)
}

/// Drop the entries of processes that are gone.
///
/// The registry is asked with this table unlocked: a lookup can hand back the
/// last reference to a process, and dropping a process is not something to do
/// while holding a lock every `prctl` needs.
fn prune() {
    let entries: Vec<(u32, u64)> = TABLE
        .lock()
        .iter()
        .map(|(&pid, &(started, _))| (pid, started))
        .collect();
    let dead: Vec<(u32, u64)> = entries
        .into_iter()
        .filter(|&(pid, started)| {
            !registry::find(pid).is_some_and(|process| process.started() == started)
        })
        .collect();
    if dead.is_empty() {
        return;
    }
    let mut table = TABLE.lock();
    for (pid, started) in dead {
        if table.get(&pid).is_some_and(|(at, _)| *at == started) {
            let _ = table.remove(&pid);
        }
    }
}

/// The process a call that takes a pid is about.
#[derive(Debug)]
pub(crate) enum Subject<'a> {
    /// The caller itself: pid 0, or its own number.
    Caller(&'a Process),
    /// Another live process.
    Other(Arc<Process>),
}

impl Deref for Subject<'_> {
    type Target = Process;

    fn deref(&self) -> &Process {
        match self {
            Subject::Caller(process) => process,
            Subject::Other(process) => process,
        }
    }
}

/// Resolve a `pid_t` argument the way `find_task_by_vpid` does: zero is the
/// caller, anything else must be a live process.
///
/// The caller is recognised by number as well as by zero because the boot
/// checks call handlers for a process that is not the running task's, and a
/// process asking about itself by its own pid must find itself either way.
///
/// # Errors
///
/// `ESRCH` for a pid nothing has, negative ones included.
#[inline(never)]
pub(crate) fn subject(process: &Process, pid: i32) -> Result<Subject<'_>, Errno> {
    if pid == 0 {
        return Ok(Subject::Caller(process));
    }
    // A number in the caller's namespace (`docs/PIDNS.md` §3).
    let number = u32::try_from(pid).map_err(|_| Errno::ESRCH)?;
    if pidns::from_user(process, number) == Some(process.pid()) {
        return Ok(Subject::Caller(process));
    }
    pidns::find_in(process, number)
        .map(Subject::Other)
        .ok_or(Errno::ESRCH)
}

/// A `pid_t` or `int` argument: 32 bits, signed, whatever the register width.
pub(crate) fn int(value: u64) -> i32 {
    value as u32 as i32
}

/// An `unsigned long` argument: the register narrowed to the native word, so
/// that a 32-bit caller's stale upper half means nothing.
pub(crate) fn ulong(value: u64) -> u64 {
    value as usize as u64
}

/// Answer `call` if it is one of this module's.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Prctl => sys_prctl(process, int(a[0]), [a[1], a[2], a[3], a[4]]),
        Syscall::Personality => Ok(sys_personality(process, a[0] as u32)),
        Syscall::Getpriority => sys_getpriority(process, int(a[0]), int(a[1])),
        Syscall::Setpriority => sys_setpriority(process, int(a[0]), int(a[1]), int(a[2])),
        Syscall::IoprioGet => sys_ioprio_get(process, int(a[0]), int(a[1])),
        Syscall::IoprioSet => sys_ioprio_set(process, int(a[0]), int(a[1]), int(a[2])),
        // Deliberately `ENOSYS`, and not an oversight: glibc registers a
        // restartable sequence at startup, and when the kernel refuses it
        // falls back to asking `getcpu`, which is answered. Accepting the
        // registration would promise to rewrite the program's instruction
        // pointer on preemption, which nothing here does.
        Syscall::Rseq => Err(Errno::ENOSYS),
        _ => return None,
    };
    Some(answer)
}

/// `prctl`'s options, from `linux/prctl.h`; verified by compiling that header
/// for x86-64 and for `arm-linux-gnueabihf`, where they agree.
mod option {
    /// Set the signal sent when the parent dies.
    pub(super) const PR_SET_PDEATHSIG: i32 = 1;
    /// Read it back, through a pointer to an `int`.
    pub(super) const PR_GET_PDEATHSIG: i32 = 2;
    /// Whether the process may dump core, as the return value.
    pub(super) const PR_GET_DUMPABLE: i32 = 3;
    /// Set it: 0 or 1 only.
    pub(super) const PR_SET_DUMPABLE: i32 = 4;
    /// Set the task's name.
    pub(super) const PR_SET_NAME: i32 = 15;
    /// Read the task's name into a 16-byte buffer.
    pub(super) const PR_GET_NAME: i32 = 16;
    /// Whether a capability is in the bounding set.
    pub(super) const PR_CAPBSET_READ: i32 = 23;
    /// Take a capability out of the bounding set, for good.
    pub(super) const PR_CAPBSET_DROP: i32 = 24;
    /// Mark the process as a reaper of orphaned descendants.
    pub(super) const PR_SET_CHILD_SUBREAPER: i32 = 36;
    /// Read that mark, through a pointer to an `int`.
    pub(super) const PR_GET_CHILD_SUBREAPER: i32 = 37;
    /// Give up gaining privileges through `execve`, irrevocably.
    pub(super) const PR_SET_NO_NEW_PRIVS: i32 = 38;
    /// Whether that was done, as the return value.
    pub(super) const PR_GET_NO_NEW_PRIVS: i32 = 39;
}

/// The highest valid signal number: Linux's `_NSIG` on all three.
const NSIG: u64 = 64;

/// `prctl`, for the options a program starting up or a busybox applet uses.
///
/// Every other option is `EINVAL`, which is what Linux answers for one it
/// does not know -- so a program probing for a newer option sees the same
/// refusal it would see on an older kernel. The argument checks are Linux's
/// own (`kernel/sys.c`): `PR_SET_NO_NEW_PRIVS` insists on exactly `1` and zero
/// in the unused arguments, because the option was designed so that a future
/// meaning for them could not be confused with a program passing garbage.
pub(crate) fn sys_prctl(process: &Process, option: i32, args: [u64; 4]) -> Result<usize, Errno> {
    let [arg2, arg3, arg4, arg5] = args.map(ulong);
    let space = process.space();
    match option {
        option::PR_SET_PDEATHSIG => {
            if arg2 > NSIG {
                return Err(Errno::EINVAL);
            }
            update(process, |a| a.parent_death_signal = arg2 as u32);
            Ok(0)
        }
        option::PR_GET_PDEATHSIG => {
            uaccess::put_u32(space, arg2, get(process).parent_death_signal).map(|()| 0)
        }
        option::PR_GET_DUMPABLE => Ok(usize::from(get(process).dumpable)),
        option::PR_SET_DUMPABLE => {
            let dumpable = match arg2 {
                0 => false,
                1 => true,
                _ => return Err(Errno::EINVAL),
            };
            update(process, |a| a.dumpable = dumpable);
            Ok(0)
        }
        option::PR_SET_NAME => {
            let name = read_name(process, arg2)?;
            update(process, |a| a.name = Some(name));
            Ok(0)
        }
        option::PR_GET_NAME => {
            let name = name_of(process);
            uaccess::copy_to_user(space, arg2, &name).map_err(|_| Errno::EFAULT)?;
            Ok(0)
        }
        option::PR_CAPBSET_READ if arg2 <= u64::from(CAP_LAST_CAP) => {
            Ok(usize::from(process.with_credentials(|held| {
                held.caps.bounding >> arg2 & 1 != 0
            })))
        }
        option::PR_CAPBSET_READ => Err(Errno::EINVAL),
        // Linux's `cap_task_prctl`: `EINVAL` past the last capability, `EPERM`
        // without `CAP_SETPCAP` in the caller's namespace. In the first
        // namespace it is recorded and reported, not enforced.
        option::PR_CAPBSET_DROP if arg2 <= u64::from(CAP_LAST_CAP) => {
            process.with_credentials(|held| {
                if !held.holds(crate::syscall::userns::CAP_SETPCAP) {
                    return Err(Errno::EPERM);
                }
                held.caps.bounding &= !(1_u64 << arg2);
                Ok(0)
            })
        }
        option::PR_CAPBSET_DROP => Err(Errno::EINVAL),
        option::PR_SET_CHILD_SUBREAPER => {
            update(process, |a| a.child_subreaper = arg2 != 0);
            Ok(0)
        }
        option::PR_GET_CHILD_SUBREAPER => {
            let reaper = u32::from(get(process).child_subreaper);
            uaccess::put_u32(space, arg2, reaper).map(|()| 0)
        }
        option::PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(Errno::EINVAL);
            }
            update(process, |a| a.no_new_privs = true);
            Ok(0)
        }
        option::PR_GET_NO_NEW_PRIVS => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(Errno::EINVAL);
            }
            Ok(usize::from(get(process).no_new_privs))
        }
        _ => Err(Errno::EINVAL),
    }
}

/// Read a name the way `strncpy_from_user` does for `PR_SET_NAME`: up to 15
/// bytes, stopping at a NUL, and never touching a byte past the NUL.
///
/// A byte at a time for that last reason. A name at the very end of a mapping
/// is legal, and a copy of all 16 bytes would fault on the page after it.
fn read_name(process: &Process, at: u64) -> Result<[u8; TASK_COMM_LEN], Errno> {
    let mut name = [0_u8; TASK_COMM_LEN];
    for (offset, slot) in (0_u64..).zip(name.iter_mut().take(TASK_COMM_LEN - 1)) {
        let mut byte = [0_u8; 1];
        let from = at.checked_add(offset).ok_or(Errno::EFAULT)?;
        uaccess::copy_from_user(process.space(), from, &mut byte).map_err(|_| Errno::EFAULT)?;
        let [value] = byte;
        if value == 0 {
            break;
        }
        *slot = value;
    }
    Ok(name)
}

/// The name `PR_GET_NAME` reports: the one set, or the program's file name.
pub(crate) fn name_of(process: &Process) -> [u8; TASK_COMM_LEN] {
    if let Some(name) = get(process).name {
        return name;
    }
    let mut name = [0_u8; TASK_COMM_LEN];
    for (slot, byte) in name.iter_mut().take(TASK_COMM_LEN - 1).zip(process.comm()) {
        *slot = byte;
    }
    name
}

/// `personality`: report the execution domain, and change it unless asked
/// only to report.
///
/// `0xffffffff` is the query, as in `kernel/exec_domain.c`. Any other value is
/// stored and the previous one returned. Nothing reads the persona -- there is
/// one execution domain, Linux's -- but `setarch` and `linux32` read it back to
/// see that it took.
pub(crate) fn sys_personality(process: &Process, persona: u32) -> usize {
    let old = if persona == u32::MAX {
        get(process).personality
    } else {
        update(process, |a| core::mem::replace(&mut a.personality, persona))
    };
    old as usize
}

/// Bytes in `struct robust_list_head` for a program whose words are `word`
/// bytes: two pointers and a `long`, so three words -- 24 on the 64-bit pair,
/// 12 on ARMv7-A and for an i386 program (`compat_robust_list_head`).
/// Verified with `sizeof` against `linux/futex.h` compiled for x86-64 and for
/// `arm-linux-gnueabihf`; AArch64 is LP64 with no override of the structure.
const fn robust_list_head(word: usize) -> u64 {
    (word * 3) as u64
}

/// `set_robust_list`: record the calling thread's head, after insisting on
/// the size.
///
/// The size check is the whole of Linux's validation, and it is there so that
/// a libc built for a different layout is refused rather than walked. Nothing
/// walks the list yet: that is for a thread that dies holding a lock.
///
/// The head is the thread's, as on Linux: a C library registers one for
/// every thread it starts and clears it as a thread unmaps its stack, and
/// kept per process one thread's clearing forgot every other's, which
/// Chromium's `ForkWithFlags` reads back through `get_robust_list`.
pub(crate) fn sys_set_robust_list(
    process: &Process,
    head: u64,
    len: u64,
    word: usize,
) -> Result<usize, Errno> {
    if ulong(len) != robust_list_head(word) {
        return Err(Errno::EINVAL);
    }
    let thread = crate::syscall::thread::current_of(process).ok_or(Errno::ESRCH)?;
    thread.set_robust_list(ulong(head));
    Ok(0)
}

/// Forget the calling thread's robust futex list, as `execve` does: its head
/// was in the memory just replaced. The process's other threads are gone by
/// then.
pub(crate) fn forget_robust_list(process: &Process) {
    if let Some(thread) = crate::syscall::thread::current_of(process) {
        thread.set_robust_list(0);
    }
}

/// The thread `get_robust_list` asks about: the caller for 0, else the thread
/// numbered `pid`, in the caller's process or any other.
fn robust_list_subject(
    process: &Process,
    pid: i32,
) -> Result<Arc<crate::syscall::thread::Thread>, Errno> {
    if pid == 0 {
        return crate::syscall::thread::current_of(process).ok_or(Errno::ESRCH);
    }
    let number = u32::try_from(pid).map_err(|_| Errno::ESRCH)?;
    let tid = pidns::from_user(process, number).ok_or(Errno::ESRCH)?;
    process
        .thread_by_tid(tid)
        .or_else(|| pidns::find_in(process, number).and_then(|other| other.thread_by_tid(tid)))
        .ok_or(Errno::ESRCH)
}

/// `get_robust_list`: the size through `len_ptr`, then the head through
/// `head_ptr`, both `word`-byte words, in that order as
/// `kernel/futex/syscalls.c` writes them.
pub(crate) fn sys_get_robust_list(
    process: &Process,
    pid: i32,
    head_ptr: u64,
    len_ptr: u64,
    word: usize,
) -> Result<usize, Errno> {
    let head = robust_list_subject(process, pid)?.robust_list();
    let put = |at: u64, value: u64| {
        let bytes = value.to_le_bytes();
        let used = bytes.get(..word).ok_or(Errno::EINVAL)?;
        uaccess::copy_to_user(process.space(), at, used).map_err(|_| Errno::EFAULT)
    };
    put(len_ptr, robust_list_head(word))?;
    put(head_ptr, head)?;
    Ok(0)
}

/// `PRIO_PROCESS`, `PRIO_PGRP` and `PRIO_USER` (`linux/resource.h`), and
/// `IOPRIO_WHO_*` (`linux/ioprio.h`), which are the same three one higher.
const PRIO_PROCESS: i32 = 0;
/// See [`PRIO_PROCESS`].
const PRIO_PGRP: i32 = 1;
/// See [`PRIO_PROCESS`].
const PRIO_USER: i32 = 2;

/// The processes a `which`/`who` pair names, for `getpriority` and its kin.
///
/// `PRIO_PROCESS` is one process. `PRIO_PGRP` is every live process in a
/// process group, the caller's own for group 0, as `kernel/sys.c` walks
/// `PIDTYPE_PGID`. `PRIO_USER` is every process of a user, and every process
/// here is root's, so uid 0 (or 0 meaning "mine") is all of them and any
/// other uid is none.
///
/// # Errors
///
/// `EINVAL` for a `which` outside the three. An empty answer is the caller's
/// to turn into `ESRCH`.
fn named_by(process: &Process, which: i32, who: i32) -> Result<Vec<Subject<'_>>, Errno> {
    match which {
        PRIO_PROCESS => Ok(subject(process, who).into_iter().collect()),
        PRIO_PGRP => {
            let group = match u32::try_from(who) {
                Ok(0) => Some(process.pgid()),
                Ok(group) => pidns::from_user(process, group),
                Err(_) => return Ok(Vec::new()),
            };
            Ok(pidns::live_in(process)?
                .into_iter()
                .filter(|member| group == Some(member.pgid()))
                .map(Subject::Other)
                .collect())
        }
        // A user's processes: those whose real uid it is, and the caller's
        // own real uid for zero.
        PRIO_USER => {
            // `who` is a user id as the caller's namespace names it; one it does
            // not map names no one.
            let uid = match u32::try_from(who) {
                Ok(0) => process.with_credentials(|ids| ids.user.real),
                Ok(uid) => match credentials::kernel_uid(process, uid) {
                    Ok(uid) => uid,
                    Err(_) => return Ok(Vec::new()),
                },
                Err(_) => return Ok(Vec::new()),
            };
            Ok(pidns::live_in(process)?
                .into_iter()
                .filter(|member| member.with_credentials(|ids| ids.user.real) == uid)
                .map(Subject::Other)
                .collect())
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `getpriority`: the best nice value among the processes named, encoded as
/// the system call encodes it.
///
/// **Not the nice value.** The system call returns `20 - nice`, from 1 to 40,
/// because a nice value can be negative and a negative return is an error;
/// the libc wrapper turns it back. A handler returning the nice value would
/// make every process look like it was running at nice 20.
pub(crate) fn sys_getpriority(process: &Process, which: i32, who: i32) -> Result<usize, Errno> {
    named_by(process, which, who)?
        .iter()
        .map(|subject| 20 - get(subject).nice)
        .max()
        .and_then(|best| usize::try_from(best).ok())
        .ok_or(Errno::ESRCH)
}

/// `setpriority`: store a nice value, clamped to -20..=19 as Linux clamps it,
/// on every process named, and give every task of each the weight it means.
///
/// Linux's `set_one_prio` for each: a process the caller does not own is
/// `EPERM`, and lowering one's nice value without privilege `EACCES`. The
/// call succeeds if any was set and no later one refused, which is the answer
/// Linux's loop leaves behind.
pub(crate) fn sys_setpriority(
    process: &Process,
    which: i32,
    who: i32,
    nice: i32,
) -> Result<usize, Errno> {
    let named = named_by(process, which, who)?;
    let nice = nice.clamp(-20, 19);
    let mut answer = Err(Errno::ESRCH);
    for subject in &named {
        if !credentials::same_owner(process, subject) {
            answer = Err(Errno::EPERM);
            continue;
        }
        if nice < get(subject).nice && credentials::require_privilege(process).is_err() {
            answer = Err(Errno::EACCES);
            continue;
        }
        update(subject, |a| a.nice = nice);
        for task in subject.tasks() {
            apply_nice(subject, &task);
        }
        if answer == Err(Errno::ESRCH) {
            answer = Ok(0);
        }
    }
    answer
}

/// Give `task` the scheduling weight `process`'s nice value means.
///
/// One task and not all of them, because the two callers have different ones
/// in hand: `setpriority` has every task the process has now, and a thread
/// starting has only itself, before anything else can see it.
///
/// A nice value outside Linux's table has no weight and is left alone, which
/// cannot happen from here -- both callers read a value already clamped to
/// -20..=19 -- and is not worth a panic if it ever does.
pub(crate) fn apply_nice(process: &Process, task: &Arc<Task>) {
    if let Some(weight) = weight_of_nice(get(process).nice) {
        crate::sched::set_weight(task, weight);
    }
}

/// Bits below the class in an I/O priority: `IOPRIO_CLASS_SHIFT`.
const IOPRIO_CLASS_SHIFT: u32 = 13;
/// `IOPRIO_CLASS_NONE`: no class chosen, which reads as one derived from nice.
const IOPRIO_CLASS_NONE: u32 = 0;
/// `IOPRIO_CLASS_RT`.
const IOPRIO_CLASS_RT: u32 = 1;
/// `IOPRIO_CLASS_BE`, best effort, every process's default.
const IOPRIO_CLASS_BE: u32 = 2;
/// `IOPRIO_CLASS_IDLE`.
const IOPRIO_CLASS_IDLE: u32 = 3;

/// The class of an I/O priority value.
fn io_class(ioprio: u32) -> u32 {
    (ioprio >> IOPRIO_CLASS_SHIFT) & 7
}

/// A process's I/O priority as Linux's `__get_task_ioprio` computes it: the
/// one set, unless it is class `NONE`, in which case best-effort at a level
/// derived from the nice value.
fn io_priority_of(process: &Process) -> u32 {
    let attributes = get(process);
    match attributes.io_priority {
        Some(ioprio) if io_class(ioprio) != IOPRIO_CLASS_NONE => ioprio,
        _ => {
            let level = u32::try_from((attributes.nice + 20) / 5).unwrap_or(0);
            (IOPRIO_CLASS_BE << IOPRIO_CLASS_SHIFT) | level
        }
    }
}

/// `ioprio_get`: the I/O priority of the processes named, the best of them if
/// several, as `block/ioprio.c` combines them.
pub(crate) fn sys_ioprio_get(process: &Process, which: i32, who: i32) -> Result<usize, Errno> {
    let named = named_by(process, which.wrapping_sub(1), who)?;
    named
        .iter()
        .map(|subject| io_priority_of(subject))
        .min()
        .map(|best| best as usize)
        .ok_or(Errno::ESRCH)
}

/// `ioprio_set`: validate the value as `ioprio_check_cap` does, then store it.
///
/// The value is checked before the processes are looked for, which is Linux's
/// order, so a bad value is `EINVAL` even for a pid that does not exist.
/// Stored and not acted on: there is no block layer to schedule.
pub(crate) fn sys_ioprio_set(
    process: &Process,
    which: i32,
    who: i32,
    ioprio: i32,
) -> Result<usize, Errno> {
    let value = ioprio as u32;
    let level = value & 7;
    match io_class(value) {
        IOPRIO_CLASS_RT | IOPRIO_CLASS_BE | IOPRIO_CLASS_IDLE => {}
        IOPRIO_CLASS_NONE if level == 0 => {}
        _ => return Err(Errno::EINVAL),
    }
    // `ioprio_check_cap`: the real-time class is root's.
    if io_class(value) == IOPRIO_CLASS_RT {
        credentials::require_privilege(process)?;
    }
    let named = named_by(process, which.wrapping_sub(1), who)?;
    if named.is_empty() {
        return Err(Errno::ESRCH);
    }
    for subject in &named {
        // `set_task_ioprio`: another user's process is `EPERM`.
        if !credentials::same_owner(process, subject) {
            return Err(Errno::EPERM);
        }
        update(subject, |a| a.io_priority = Some(value));
    }
    Ok(0)
}
