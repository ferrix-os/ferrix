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
//! # The scheduler is asked nothing
//!
//! Affinity, policy and priority are validated as Linux validates them and
//! then not applied, and each handler says so. The one scheduling fact that is
//! real is the set of processors, and that is what `sched_getaffinity` reports.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_vfs::fd::MAX_LIMIT;

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

/// The logical numbers of the processors that are running and in service.
fn online_cpus() -> Vec<usize> {
    match smp::topology() {
        Some(topology) => topology
            .cpus()
            .iter()
            .filter(|cpu| cpu.in_service())
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

/// `SCHED_OTHER`, the only policy this kernel runs anything under.
const SCHED_OTHER: i32 = 0;

/// The pid checks every `sched_*param` and `sched_*scheduler` call makes
/// first: a null structure or a negative pid is `EINVAL` before any lookup.
fn param_args(pid: i32, at: u64) -> Result<(), Errno> {
    if at == 0 || pid < 0 {
        return Err(Errno::EINVAL);
    }
    Ok(())
}

/// `sched_getparam`: `struct sched_param` is one `int`, and under
/// `SCHED_OTHER` its priority is always zero.
pub(crate) fn sys_sched_getparam(process: &Process, pid: i32, at: u64) -> Result<usize, Errno> {
    param_args(pid, at)?;
    let _target = subject(process, pid)?;
    uaccess::put_u32(process.space(), at, 0).map(|()| 0)
}

/// `sched_setparam`: only priority zero is valid under `SCHED_OTHER`, so that
/// is all that is accepted.
pub(crate) fn sys_sched_setparam(process: &Process, pid: i32, at: u64) -> Result<usize, Errno> {
    param_args(pid, at)?;
    let priority = uaccess::get_u32(process.space(), at)?;
    let target = subject(process, pid)?;
    if priority != 0 {
        return Err(Errno::EINVAL);
    }
    if !credentials::same_owner(process, &target) {
        return Err(Errno::EPERM);
    }
    Ok(0)
}

/// `sched_getscheduler`: `SCHED_OTHER`, for every process.
pub(crate) fn sys_sched_getscheduler(process: &Process, pid: i32) -> Result<usize, Errno> {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let _target = subject(process, pid)?;
    Ok(0)
}

/// `sched_setscheduler`: `SCHED_OTHER` at priority zero is accepted, being
/// what every process already has; anything else is `EINVAL`.
///
/// Root on Linux could move itself to `SCHED_FIFO`. Here that would be a
/// promise -- run me before every fair task, and preempt them to do it -- that
/// the scheduler has no class to keep, so it is refused rather than accepted
/// and ignored.
pub(crate) fn sys_sched_setscheduler(
    process: &Process,
    pid: i32,
    policy: i32,
    at: u64,
) -> Result<usize, Errno> {
    if policy < 0 {
        return Err(Errno::EINVAL);
    }
    param_args(pid, at)?;
    let priority = uaccess::get_u32(process.space(), at)?;
    let target = subject(process, pid)?;
    if policy != SCHED_OTHER || priority != 0 {
        return Err(Errno::EINVAL);
    }
    if !credentials::same_owner(process, &target) {
        return Err(Errno::EPERM);
    }
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

/// `sched_rr_get_interval`, and its `time64` form on ARMv7-A.
///
/// Zero, which is what Linux reports for a task whose policy has no fixed
/// round-robin quantum and whose run queue carries no load to divide: the
/// fair scheduler's slices are computed per decision, not held per task.
pub(crate) fn sys_sched_rr_get_interval(
    process: &Process,
    pid: i32,
    at: u64,
    width: TimeWidth,
) -> Result<usize, Errno> {
    if pid < 0 {
        return Err(Errno::EINVAL);
    }
    let _target = subject(process, pid)?;
    time::write_pair(process, at, 0, 0, width)?;
    Ok(0)
}
