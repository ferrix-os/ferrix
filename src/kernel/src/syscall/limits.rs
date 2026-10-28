//! What a process may use: resource limits, the processors it may run on,
//! and the scheduling policy it runs under.
//!
//! # One limit is real
//!
//! `RLIMIT_NOFILE` is the descriptor table's own limit, read from and written
//! to the table, so a program that lowers it gets `EMFILE` where it should and
//! a program that raises it can open more. The others are kept and reported
//! (see `attributes`) and enforced by nothing yet: `RLIMIT_STACK` reads as
//! Linux's 8 MiB default because that number sizes thread stacks in every libc
//! and a shell prints it for `ulimit -s`, and the rest read as unlimited,
//! which is the truth about a kernel that does not count those resources.
//!
//! # The scheduler
//!
//! Policy and priority are the scheduler's: `SCHED_FIFO` and `SCHED_RR`, in
//! a build with a tier above fair, move a thread's task to that tier, under
//! Linux's permission rule (`docs/SCHED-CLASSES.md` §9e). Affinity is
//! validated as Linux validates it and then not applied, as its handler
//! says; the processors themselves are real, and that is what
//! `sched_getaffinity` reports.

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_sched_class::{RtParams, RtPolicy};
use ferrix_vfs::fd::MAX_LIMIT;

use crate::sched;
use crate::smp;
use crate::syscall::attributes::{self, Limit, RLIM_NLIMITS, int, subject};
use crate::syscall::credentials;
use crate::syscall::process::Process;
use crate::syscall::time::{self, TimeWidth};
use crate::syscall::uaccess::{self, WORD};

/// `RLIMIT_STACK` in `asm-generic/resource.h`; the same on all three.
const RLIMIT_STACK: u32 = 3;
/// `RLIMIT_NOFILE`, likewise.
const RLIMIT_NOFILE: u32 = 7;
/// `RLIM64_INFINITY`: no limit.
const RLIM_INFINITY: u64 = u64::MAX;
/// `_STK_LIM`, Linux's default soft stack limit.
const STACK_DEFAULT: u64 = 8 << 20;
/// Linux's default hard descriptor limit, `INR_OPEN_MAX`. The soft default,
/// `INR_OPEN_CUR`, is the table's own default.
const NOFILE_HARD_DEFAULT: u64 = 4096;

/// Answer `call` if it is one of this module's.
pub(crate) fn dispatch(
    call: Syscall,
    a: &[u64; 6],
    process: &Process,
) -> Option<Result<usize, Errno>> {
    let answer = match call {
        Syscall::Prlimit64 => sys_prlimit64(process, int(a[0]), a[1] as u32, a[2], a[3]),
        Syscall::SchedSetaffinity => sys_sched_setaffinity(process, int(a[0]), a[1] as u32, a[2]),
        Syscall::SchedGetparam => sys_sched_getparam(process, int(a[0]), a[1]),
        Syscall::SchedSetparam => sys_sched_setparam(process, int(a[0]), a[1]),
        Syscall::SchedGetscheduler => sys_sched_getscheduler(process, int(a[0])),
        Syscall::SchedSetscheduler => sys_sched_setscheduler(process, int(a[0]), int(a[1]), a[2]),
        Syscall::SchedSetattr => sys_sched_setattr(process, int(a[0]), a[1], a[2] as u32),
        Syscall::SchedGetattr => {
            sys_sched_getattr(process, int(a[0]), a[1], a[2] as u32, a[3] as u32)
        }
        Syscall::SchedGetPriorityMax => priority_range(int(a[0])).map(|(_, max)| max),
        Syscall::SchedGetPriorityMin => priority_range(int(a[0])).map(|(min, _)| min),
        Syscall::SchedRrGetInterval => {
            sys_sched_rr_get_interval(process, int(a[0]), a[1], TimeWidth::Native)
        }
        Syscall::SchedRrGetIntervalTime64 => {
            sys_sched_rr_get_interval(process, int(a[0]), a[1], TimeWidth::Wide)
        }
        _ => return None,
    };
    Some(answer)
}

/// A resource number as an index, or `EINVAL` past `RLIM_NLIMITS`.
fn index_of(resource: u32) -> Result<usize, Errno> {
    usize::try_from(resource)
        .ok()
        .filter(|&index| index < RLIM_NLIMITS)
        .ok_or(Errno::EINVAL)
}

/// The limit `process` is under for `resource`.
///
/// # Errors
///
/// `EINVAL` for a resource Linux does not have.
pub(crate) fn limit_of(process: &Process, resource: u32) -> Result<Limit, Errno> {
    let index = index_of(resource)?;
    let stored = attributes::get(process)
        .limits
        .get(index)
        .copied()
        .flatten();
    Ok(match resource {
        RLIMIT_NOFILE => {
            let soft = u64::from(process.files().lock().limit());
            let hard = stored.map_or(NOFILE_HARD_DEFAULT, |limit| limit.hard);
            Limit {
                soft,
                hard: hard.max(soft),
            }
        }
        RLIMIT_STACK => stored.unwrap_or(Limit {
            soft: STACK_DEFAULT,
            hard: RLIM_INFINITY,
        }),
        // Linux's default: no real-time priority without `CAP_SYS_NICE`.
        RLIMIT_RTPRIO => stored.unwrap_or(Limit { soft: 0, hard: 0 }),
        _ => stored.unwrap_or(Limit {
            soft: RLIM_INFINITY,
            hard: RLIM_INFINITY,
        }),
    })
}

/// Put `process` under `new` for `resource` at `caller`'s asking, with
/// `do_prlimit`'s checks.
///
/// Raising a hard limit needs `CAP_SYS_RESOURCE`: root's, `EPERM` for anyone
/// else. `RLIMIT_NOFILE` also has a ceiling, `nr_open`, past which even root
/// is `EPERM`.
fn set_limit(caller: &Process, process: &Process, resource: u32, new: Limit) -> Result<(), Errno> {
    let index = index_of(resource)?;
    if new.soft > new.hard {
        return Err(Errno::EINVAL);
    }
    if new.hard > limit_of(process, resource)?.hard {
        credentials::require_privilege(caller)?;
    }
    if resource == RLIMIT_NOFILE {
        if new.hard > u64::from(MAX_LIMIT) {
            return Err(Errno::EPERM);
        }
        let soft = u32::try_from(new.soft).map_err(|_| Errno::EPERM)?;
        process
            .files()
            .lock()
            .set_limit(soft)
            .map_err(|_| Errno::EPERM)?;
    }
    attributes::update(process, |a| {
        if let Some(slot) = a.limits.get_mut(index) {
            *slot = Some(new);
        }
    });
    Ok(())
}

/// A limit as a `struct rlimit` of `word`-byte words can carry it: in 32 bits
/// anything wider is `RLIM_INFINITY`, which is `~0UL`, as `kernel/sys.c` and
/// `compat_sys_getrlimit` clamp it. The identity for a 64-bit program.
fn to_word(value: u64, word: usize) -> u64 {
    if word == 4 && value > u64::from(u32::MAX) {
        u64::from(u32::MAX)
    } else {
        value
    }
}

/// The inverse: a 32-bit `RLIM_INFINITY` means the 64-bit one.
fn from_word(value: u64, word: usize) -> u64 {
    if word == 4 && value == u64::from(u32::MAX) {
        RLIM_INFINITY
    } else {
        value
    }
}

/// `getrlimit`, and ARMv7-A's `ugetrlimit`, which the table folds onto it.
///
/// `struct rlimit` is two `unsigned long`s: 16 bytes on the 64-bit pair and 8
/// on ARMv7-A, by `sizeof` against `linux/resource.h` compiled for x86-64 and
/// `arm-linux-gnueabihf`. (ARM's original `getrlimit`, which clamped
/// differently, is not in the EABI table at all.)
pub(crate) fn sys_getrlimit(process: &Process, resource: u32, at: u64) -> Result<usize, Errno> {
    sys_getrlimit_at_width(process, resource, at, WORD)
}

/// [`sys_getrlimit`] into a `struct rlimit` of `word`-byte words: an i386
/// program's `ugetrlimit` takes two 32-bit ones.
pub(crate) fn sys_getrlimit_at_width(
    process: &Process,
    resource: u32,
    at: u64,
    word: usize,
) -> Result<usize, Errno> {
    let limit = limit_of(process, resource)?;
    let mut bytes = [0_u8; 16];
    let fields = to_word(limit.soft, word)
        .to_le_bytes()
        .into_iter()
        .take(word)
        .chain(
            to_word(limit.hard, word)
                .to_le_bytes()
                .into_iter()
                .take(word),
        );
    for (slot, byte) in bytes.iter_mut().zip(fields) {
        *slot = byte;
    }
    let record = bytes.get(..word * 2).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), at, record).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `setrlimit`: the structure is read before the resource is checked, which is
/// Linux's order.
pub(crate) fn sys_setrlimit(process: &Process, resource: u32, at: u64) -> Result<usize, Errno> {
    sys_setrlimit_at_width(process, resource, at, WORD)
}

/// [`sys_setrlimit`] from a `struct rlimit` of `word`-byte words.
pub(crate) fn sys_setrlimit_at_width(
    process: &Process,
    resource: u32,
    at: u64,
    word: usize,
) -> Result<usize, Errno> {
    let mut bytes = [0_u8; 16];
    let record = bytes.get_mut(..word * 2).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, record).map_err(|_| Errno::EFAULT)?;
    let field = |index: usize| {
        let mut eight = [0_u8; 8];
        for (slot, byte) in eight
            .iter_mut()
            .zip(bytes.iter().skip(index * word).take(word))
        {
            *slot = *byte;
        }
        from_word(u64::from_le_bytes(eight), word)
    };
    let (soft, hard) = (field(0), field(1));
    set_limit(process, process, resource, Limit { soft, hard }).map(|()| 0)
}

/// `prlimit64`: `struct rlimit64`, two `__u64`s and 16 bytes on every
/// architecture, for the caller or any process.
///
/// Linux's order throughout: the new limit is read, the process found, the
/// resource checked, the old limit taken, the new one applied, and only then
/// the old one written -- so a bad `old` pointer is `EFAULT` *after* the change
/// has been made, as it is there.
pub(crate) fn sys_prlimit64(
    process: &Process,
    pid: i32,
    resource: u32,
    new_at: u64,
    old_at: u64,
) -> Result<usize, Errno> {
    let space = process.space();
    let new = if new_at == 0 {
        None
    } else {
        let mut bytes = [0_u8; 16];
        uaccess::copy_from_user(space, new_at, &mut bytes).map_err(|_| Errno::EFAULT)?;
        let (soft, hard) = bytes.split_at(8);
        Some(Limit {
            soft: u64::from_le_bytes(soft.try_into().map_err(|_| Errno::EFAULT)?),
            hard: u64::from_le_bytes(hard.try_into().map_err(|_| Errno::EFAULT)?),
        })
    };
    let target = subject(process, pid)?;
    if !credentials::same_ids(process, &target) {
        return Err(Errno::EPERM);
    }
    let old = limit_of(&target, resource)?;
    if let Some(new) = new {
        set_limit(process, &target, resource, new)?;
    }
    if old_at != 0 {
        let mut bytes = [0_u8; 16];
        for (slot, byte) in bytes.iter_mut().zip(
            old.soft
                .to_le_bytes()
                .into_iter()
                .chain(old.hard.to_le_bytes()),
        ) {
            *slot = byte;
        }
        uaccess::copy_to_user(space, old_at, &bytes).map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// Bytes in this kernel's CPU mask: enough whole `unsigned long`s for every
/// processor the machine has, which is what Linux's `cpumask_size()` is for a
/// kernel sized to its machine.
fn mask_bytes() -> usize {
    smp::count().div_ceil(WORD * 8) * WORD
}

/// The logical numbers of the processors that are running.
fn online_cpus() -> Vec<usize> {
    match smp::topology() {
        Some(topology) => topology
            .cpus()
            .iter()
            .filter(|cpu| cpu.is_online())
            .map(|cpu| cpu.logical)
            .collect(),
        None => vec![0],
    }
}

/// `sched_getaffinity`: the processors that are running, as a bit mask.
///
/// **Returns the number of bytes written, not zero.** The raw system call
/// does; the glibc wrapper zeroes the rest of the caller's buffer and returns
/// zero itself, and a handler that returned zero would have it zero the whole
/// mask. The length must cover every processor and be a whole number of
/// the caller's `unsigned long`s, `word` bytes each, which is how
/// `kernel/sched/syscalls.c` and its compat twin in `kernel/compat.c` refuse
/// a buffer that could not hold the answer. The count returned is this
/// kernel's mask size at either width, as both of Linux's are.
pub(crate) fn sys_sched_getaffinity(
    process: &Process,
    pid: i32,
    len: u32,
    at: u64,
    word: usize,
) -> Result<usize, Errno> {
    let len = usize::try_from(len).map_err(|_| Errno::EINVAL)?;
    if len.saturating_mul(8) < smp::count() || len % word != 0 {
        return Err(Errno::EINVAL);
    }
    let _target = subject(process, pid)?;
    let size = len.min(mask_bytes());
    let mut mask = vec![0_u8; size];
    for cpu in online_cpus() {
        if let Some(byte) = mask.get_mut(cpu / 8) {
            *byte |= 1 << (cpu % 8);
        }
    }
    uaccess::copy_to_user(process.space(), at, &mask).map_err(|_| Errno::EFAULT)?;
    Ok(size)
}

/// `sched_setaffinity`: validated, accepted, and **not applied**.
///
/// The mask is read as Linux reads it (a short one is zero-extended, a long
/// one truncated) and must name at least one running processor, or it is
/// `EINVAL`. The task is not moved and not pinned: the scheduler has no
/// per-task affinity to set, and a program told its mask took will at worst
/// run somewhere it did not ask to, which is what a machine with one
/// processor does to every affinity request anyway.
pub(crate) fn sys_sched_setaffinity(
    process: &Process,
    pid: i32,
    len: u32,
    at: u64,
) -> Result<usize, Errno> {
    let size = mask_bytes();
    let mut mask = vec![0_u8; size];
    let take = usize::try_from(len).unwrap_or(usize::MAX).min(size);
    let wanted = mask.get_mut(..take).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, wanted).map_err(|_| Errno::EFAULT)?;
    let target = subject(process, pid)?;
    if !credentials::same_owner(process, &target) {
        return Err(Errno::EPERM);
    }
    let runnable = online_cpus().into_iter().any(|cpu| {
        mask.get(cpu / 8)
            .is_some_and(|byte| byte & (1 << (cpu % 8)) != 0)
    });
    if !runnable {
        return Err(Errno::EINVAL);
    }
    Ok(0)
}

/// `SCHED_OTHER`.
const SCHED_OTHER: i32 = 0;
/// `SCHED_FIFO`.
const SCHED_FIFO: i32 = 1;
/// `SCHED_RR`.
const SCHED_RR: i32 = 2;
/// `SCHED_RESET_ON_FORK`, or'ed into a policy.
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;
/// `RLIMIT_RTPRIO`: the highest real-time priority a task may raise itself
/// to without `CAP_SYS_NICE`.
const RLIMIT_RTPRIO: u32 = 14;

/// The thread a `sched_*` call names: the caller for 0, else the thread
/// numbered `pid` (a thread id, as Linux's calls take), after the checks
/// every one makes first: a negative pid is `EINVAL`, and so is a null
/// structure where `at` is one.
fn sched_subject(
    process: &Process,
    pid: i32,
    at: Option<u64>,
) -> Result<Arc<crate::syscall::thread::Thread>, Errno> {
    if pid < 0 || at == Some(0) {
        return Err(Errno::EINVAL);
    }
    attributes::robust_list_subject(process, pid)
}

/// A thread's policy and priority, as the calls answer them: its task's, or
/// `SCHED_OTHER` for a thread with no live task.
fn policy_of(thread: &crate::syscall::thread::Thread) -> (i32, u32, bool) {
    let Some(task) = attributes::task_of(thread) else {
        return (SCHED_OTHER, 0, false);
    };
    let word = task.rt_word();
    let (policy, priority) = match word.params() {
        Some(RtParams {
            policy: RtPolicy::Fifo,
            priority,
        }) => (SCHED_FIFO, u32::from(priority)),
        Some(RtParams {
            policy: RtPolicy::Rr,
            priority,
        }) => (SCHED_RR, u32::from(priority)),
        None => (SCHED_OTHER, 0),
    };
    (policy, priority, word.resets_on_fork())
}

/// A policy and priority as the calls take them, checked as Linux's
/// `__sched_setscheduler` checks them (test-rtsched's errors step): `SCHED_OTHER` at 0,
/// `SCHED_FIFO` and `SCHED_RR` at 1 to 99, and only in a build with a tier
/// above fair (L.sched.116); every other policy -- `SCHED_BATCH`,
/// `SCHED_IDLE`, `SCHED_DEADLINE`, `SCHED_EXT` -- is `EINVAL`.
fn checked_params(policy: i32, priority: u32) -> Result<Option<RtParams>, Errno> {
    let rt = |policy| {
        let priority = u8::try_from(priority).map_err(|_| Errno::EINVAL)?;
        let params = RtParams { policy, priority };
        if !params.valid() || !sched::realtime_built() {
            return Err(Errno::EINVAL);
        }
        Ok(Some(params))
    };
    match policy {
        SCHED_OTHER if priority == 0 => Ok(None),
        SCHED_FIFO => rt(RtPolicy::Fifo),
        SCHED_RR => rt(RtPolicy::Rr),
        _ => Err(Errno::EINVAL),
    }
}

/// Linux's permission rule for a change of policy (`user_check_sched_setscheduler`,
/// test-rtsched's permission step): with `CAP_SYS_NICE` anything; otherwise only a target the
/// caller owns, a real-time policy only with a nonzero `RLIMIT_RTPRIO` when
/// it is a new policy, a priority raised only within that limit, and the
/// reset-on-fork flag never cleared. Lowering a task to `SCHED_OTHER`, or
/// its priority, needs only ownership.
fn may_set_policy(
    caller: &Process,
    thread: &crate::syscall::thread::Thread,
    new: Option<RtParams>,
    reset_on_fork: bool,
) -> Result<(), Errno> {
    if credentials::require_privilege(caller).is_ok() {
        return Ok(());
    }
    let target = thread.process();
    if !credentials::same_owner(caller, target) {
        return Err(Errno::EPERM);
    }
    let (policy, priority, resets) = policy_of(thread);
    if let Some(new) = new {
        let limit = limit_of(target, RLIMIT_RTPRIO)?.soft;
        let new_policy = match new.policy {
            RtPolicy::Fifo => SCHED_FIFO,
            RtPolicy::Rr => SCHED_RR,
        };
        if new_policy != policy && limit == 0 {
            return Err(Errno::EPERM);
        }
        let raised = u64::from(new.priority);
        if raised > u64::from(priority) && raised > limit {
            return Err(Errno::EPERM);
        }
    }
    if resets && !reset_on_fork {
        return Err(Errno::EPERM);
    }
    Ok(())
}

/// Apply a checked policy to `thread`'s task (L.sched.112): moved between
/// tiers under its queue's lock, at the tail of its new place.
fn apply_policy(
    thread: &crate::syscall::thread::Thread,
    params: Option<RtParams>,
    reset_on_fork: bool,
) -> Result<usize, Errno> {
    let Some(task) = attributes::task_of(thread) else {
        return Err(Errno::ESRCH);
    };
    match sched::set_policy(&task, params, reset_on_fork) {
        Ok(()) => Ok(0),
        Err(sched::PolicyRefused::NoTier) => Err(Errno::EINVAL),
        Err(sched::PolicyRefused::NoMemory) => Err(Errno::ENOMEM),
    }
}

/// `sched_getparam`: `struct sched_param` is one `int`, the thread's
/// real-time priority, zero under `SCHED_OTHER`.
pub(crate) fn sys_sched_getparam(process: &Process, pid: i32, at: u64) -> Result<usize, Errno> {
    let thread = sched_subject(process, pid, Some(at))?;
    let (_, priority, _) = policy_of(&thread);
    uaccess::put_u32(process.space(), at, priority).map(|()| 0)
}

/// `sched_setparam`: a new priority under the thread's own policy.
pub(crate) fn sys_sched_setparam(process: &Process, pid: i32, at: u64) -> Result<usize, Errno> {
    if pid < 0 || at == 0 {
        return Err(Errno::EINVAL);
    }
    let priority = uaccess::get_u32(process.space(), at)?;
    let thread = sched_subject(process, pid, Some(at))?;
    let (policy, _, resets) = policy_of(&thread);
    let params = checked_params(policy, priority)?;
    may_set_policy(process, &thread, params, resets)?;
    apply_policy(&thread, params, resets)
}

/// `sched_getscheduler`: the thread's policy, with `SCHED_RESET_ON_FORK`
/// or'ed in when it is set.
pub(crate) fn sys_sched_getscheduler(process: &Process, pid: i32) -> Result<usize, Errno> {
    let thread = sched_subject(process, pid, None)?;
    let (policy, _, resets) = policy_of(&thread);
    let reset = if resets { SCHED_RESET_ON_FORK } else { 0 };
    usize::try_from(policy | reset).map_err(|_| Errno::EINVAL)
}

/// `sched_setscheduler`: `SCHED_OTHER` at 0, or `SCHED_FIFO` or `SCHED_RR`
/// at 1 to 99 in a build with a tier above fair, with
/// `SCHED_RESET_ON_FORK` or'ed in for the flag (docs/SCHED-CLASSES.md §9e).
pub(crate) fn sys_sched_setscheduler(
    process: &Process,
    pid: i32,
    policy: i32,
    at: u64,
) -> Result<usize, Errno> {
    if policy < 0 || pid < 0 || at == 0 {
        return Err(Errno::EINVAL);
    }
    let reset_on_fork = policy & SCHED_RESET_ON_FORK != 0;
    let policy = policy & !SCHED_RESET_ON_FORK;
    let priority = uaccess::get_u32(process.space(), at)?;
    let thread = sched_subject(process, pid, Some(at))?;
    let params = checked_params(policy, priority)?;
    may_set_policy(process, &thread, params, reset_on_fork)?;
    apply_policy(&thread, params, reset_on_fork)
}

/// `sched_attr`'s first size, `SCHED_ATTR_SIZE_VER0`: up to its period.
const SCHED_ATTR_SIZE_VER0: u32 = 48;
/// Its size with the utilisation clamps, `SCHED_ATTR_SIZE_VER1`: all of it.
const SCHED_ATTR_SIZE_VER1: u32 = 56;
/// The most `sched_setattr` reads of a larger structure, as Linux's
/// `sched_copy_attr`: a page, past which it is `E2BIG`.
const SCHED_ATTR_MAX: u32 = 4096;
/// `SCHED_FLAG_RESET_ON_FORK`.
const SCHED_FLAG_RESET_ON_FORK: u64 = 0x01;
/// `SCHED_FLAG_KEEP_POLICY`: the policy stays as it is.
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
/// `SCHED_FLAG_KEEP_PARAMS`: the priority and the nice value stay.
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
/// `SCHED_FLAG_UTIL_CLAMP_MIN` and `_MAX`: no utilisation clamps here, which
/// Linux built without them answers `EOPNOTSUPP`.
const SCHED_FLAG_UTIL_CLAMP: u64 = 0x60;
/// Every flag Linux knows, `SCHED_FLAG_ALL`: another is `EINVAL`.
const SCHED_FLAG_ALL: u64 = 0x7f;

/// `sched_setattr`: as `sched_setscheduler`, from a `struct sched_attr`,
/// read as Linux's `sched_copy_attr` reads it (test-rtsched's errors step): a size of zero
/// is the first size, one below it or above a page `E2BIG` with the size
/// this kernel knows written back, and bytes past that size must be zero.
/// Under `SCHED_OTHER` its nice value is the thread's, as `setpriority`
/// sets it.
pub(crate) fn sys_sched_setattr(
    process: &Process,
    pid: i32,
    at: u64,
    flags: u32,
) -> Result<usize, Errno> {
    if pid < 0 || at == 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }
    let space = process.space();
    let mut size = uaccess::get_u32(space, at)?;
    if size == 0 {
        size = SCHED_ATTR_SIZE_VER0;
    }
    if !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX).contains(&size) {
        uaccess::put_u32(space, at, SCHED_ATTR_SIZE_VER1)?;
        return Err(Errno::E2BIG);
    }
    let mut attr = [0_u8; SCHED_ATTR_SIZE_VER1 as usize];
    let known = size.min(SCHED_ATTR_SIZE_VER1) as usize;
    let head = attr.get_mut(..known).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(space, at, head).map_err(|_| Errno::EFAULT)?;
    // Anything past the structure this kernel knows must be zero.
    let mut past = SCHED_ATTR_SIZE_VER1;
    while past < size {
        let mut chunk = [0_u8; 64];
        let len = (size - past).min(64) as usize;
        let chunk = chunk.get_mut(..len).ok_or(Errno::EINVAL)?;
        uaccess::copy_from_user(space, at + u64::from(past), chunk).map_err(|_| Errno::EFAULT)?;
        if chunk.iter().any(|&byte| byte != 0) {
            uaccess::put_u32(space, at, SCHED_ATTR_SIZE_VER1)?;
            return Err(Errno::E2BIG);
        }
        past += len as u32;
    }
    let word = |at: usize| {
        attr.get(at..at + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_ne_bytes)
            .unwrap_or(0)
    };
    let policy = word(4) as i32;
    let sched_flags = u64::from(word(8)) | (u64::from(word(12)) << 32);
    let nice = (word(16) as i32).clamp(-20, 19);
    let priority = word(20);
    if sched_flags & !SCHED_FLAG_ALL != 0 {
        return Err(Errno::EINVAL);
    }
    if sched_flags & SCHED_FLAG_UTIL_CLAMP != 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    if policy < 0 {
        return Err(Errno::EINVAL);
    }
    let thread = sched_subject(process, pid, Some(at))?;
    let (current, current_priority, _) = policy_of(&thread);
    let reset_on_fork = sched_flags & SCHED_FLAG_RESET_ON_FORK != 0;
    let policy = if sched_flags & SCHED_FLAG_KEEP_POLICY != 0 {
        current
    } else {
        policy
    };
    let keep_params = sched_flags & SCHED_FLAG_KEEP_PARAMS != 0;
    let priority = if keep_params && policy == current {
        current_priority
    } else {
        priority
    };
    let params = checked_params(policy, priority)?;
    may_set_policy(process, &thread, params, reset_on_fork)?;
    // A nice value lowered needs the privilege `setpriority` asks for.
    let set_nice = params.is_none() && !keep_params && nice != thread.nice();
    if set_nice && nice < thread.nice() && credentials::require_privilege(process).is_err() {
        return Err(Errno::EPERM);
    }
    let _ = apply_policy(&thread, params, reset_on_fork)?;
    if set_nice {
        thread.set_nice(nice);
        if let Some(task) = attributes::task_of(&thread) {
            attributes::apply_nice(thread.nice(), &task);
        }
    }
    Ok(0)
}

/// `sched_getattr`: the thread's policy, flags, nice value and priority in a
/// `struct sched_attr` of at most `size` bytes, its size field the bytes
/// written (test-rtsched's errors step): a size below the first one, above a page, or
/// nonzero flags are `EINVAL`.
pub(crate) fn sys_sched_getattr(
    process: &Process,
    pid: i32,
    at: u64,
    size: u32,
    flags: u32,
) -> Result<usize, Errno> {
    if pid < 0 || at == 0 || flags != 0 || !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_MAX).contains(&size)
    {
        return Err(Errno::EINVAL);
    }
    let thread = sched_subject(process, pid, Some(at))?;
    let (policy, priority, resets) = policy_of(&thread);
    let written = size.min(SCHED_ATTR_SIZE_VER1);
    let mut attr = [0_u8; SCHED_ATTR_SIZE_VER1 as usize];
    let mut put = |at: usize, value: u32| {
        if let Some(bytes) = attr.get_mut(at..at + 4) {
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
    };
    put(0, written);
    put(4, policy as u32);
    put(8, u32::from(resets) * SCHED_FLAG_RESET_ON_FORK as u32);
    put(16, thread.nice() as u32);
    put(20, priority);
    let out = attr.get(..written as usize).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), at, out).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// `sched_get_priority_min` and `_max`, as Linux defines them for every
/// policy it has: 1 to 99 for `SCHED_FIFO` and `SCHED_RR`, zero for
/// `SCHED_OTHER`, `SCHED_BATCH`, `SCHED_IDLE` and `SCHED_DEADLINE`. These are
/// facts about the ABI rather than about this scheduler, so all six are
/// answered even though only one can be set.
fn priority_range(policy: i32) -> Result<(usize, usize), Errno> {
    match policy {
        1 | 2 => Ok((1, 99)),
        0 | 3 | 5 | 6 => Ok((0, 0)),
        _ => Err(Errno::EINVAL),
    }
}

/// `sched_rr_get_interval`, and its `time64` form on ARMv7-A: the
/// round-robin quantum for a `SCHED_RR` thread (100 ms, R13), and zero for
/// any other, which is what Linux reports for a `SCHED_FIFO` task and, here,
/// for a fair one, whose slices are computed per decision, not held per task.
pub(crate) fn sys_sched_rr_get_interval(
    process: &Process,
    pid: i32,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    let thread = sched_subject(process, pid, None)?;
    let quantum = match policy_of(&thread) {
        (SCHED_RR, _, _) => sched::rr_quantum_ns(),
        _ => 0,
    };
    time::write_pair(
        process,
        at,
        quantum / 1_000_000_000,
        quantum % 1_000_000_000,
        width,
    )?;
    Ok(0)
}
