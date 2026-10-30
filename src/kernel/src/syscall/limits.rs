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
//! # What the scheduler is asked
//!
//! A thread's processors (`sched_setaffinity`, applied and read back as
//! Linux 7.0 does: `docs/GAME-MODE-SCHED.md` §2.1) and its slice request
//! (`sched_setattr`, §2.2). Policy and priority are validated as Linux
//! validates them and then not applied, and each handler says so.

use alloc::vec;
use alloc::vec::Vec;

use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::nr::Syscall;
use ferrix_vfs::fd::MAX_LIMIT;

use ferrix_sched::CpuSet;

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
        Syscall::SchedSetattr => sys_sched_setattr(process, int(a[0]), a[1], a[2]),
        Syscall::SchedGetattr => sys_sched_getattr(process, int(a[0]), a[1], a[2] as u32, a[3]),
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

/// The mask `sched_setaffinity` was given, `len` bytes at `at`, read as
/// Linux reads it: a short one zero-extended, a long one truncated to this
/// kernel's mask size.
fn read_mask(process: &Process, len: u32, at: u64) -> Result<CpuSet, Errno> {
    let size = mask_bytes();
    let mut mask = vec![0_u8; size];
    let take = usize::try_from(len).unwrap_or(usize::MAX).min(size);
    let wanted = mask.get_mut(..take).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, wanted).map_err(|_| Errno::EFAULT)?;
    let mut set = CpuSet::empty();
    for (index, byte) in mask.iter().enumerate() {
        for bit in 0..8 {
            if byte & (1 << bit) != 0 {
                // Past the set's processors is no processor at all, and
                // names nothing, as an offline one does.
                let _ = set.insert(index * 8 + bit);
            }
        }
    }
    Ok(set)
}

/// `sched_getaffinity`: the processors the thread may run on now, as a bit
/// mask -- its effective mask, the one it was given and in service, or
/// every processor in service while that is none (`sched::effective_mask`,
/// L.sched.123). A thread named by a tid in the caller's pid namespace, as
/// Linux's pid here is a thread's; a process with no thread yet (a boot
/// check's) reads every processor in service.
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
    let effective = match attributes::thread_named(process, pid) {
        Some(thread) => attributes::task_of(&thread).map(|task| sched::effective_mask(&task)),
        None => {
            let _target = subject(process, pid)?;
            None
        }
    };
    let size = len.min(mask_bytes());
    let mut mask = vec![0_u8; size];
    let cpus = match effective {
        Some(set) => set.iter().collect(),
        None => online_cpus(),
    };
    for cpu in cpus {
        if let Some(byte) = mask.get_mut(cpu / 8) {
            *byte |= 1 << (cpu % 8);
        }
    }
    uaccess::copy_to_user(process.space(), at, &mask).map_err(|_| Errno::EFAULT)?;
    Ok(size)
}

/// `sched_setaffinity`: the thread named runs only on the processors of the
/// mask from now on (`docs/GAME-MODE-SCHED.md` §2.1, L.sched.96-101).
///
/// As Linux 7.0's: the mask is read as [`read_mask`] reads it; the tid is
/// the caller's pid namespace's, a thread's (`ESRCH` for none, and for a
/// native process's, which no Linux call names); `EPERM` without
/// `check_same_owner`'s permission (`credentials::same_owner`); the mask
/// kept is the one given, in the domain and in service, and `EINVAL` when
/// that is none (`cpu_active_mask`). The call returns once the thread is
/// off any processor the mask excludes (`sched::set_affinity`). A process
/// with no thread yet (a boot check's) has no task to place, and the mask
/// is only validated.
pub(crate) fn sys_sched_setaffinity(
    process: &Process,
    pid: i32,
    len: u32,
    at: u64,
) -> Result<usize, Errno> {
    let mask = read_mask(process, len, at)?;
    let Some(thread) = attributes::thread_named(process, pid) else {
        let target = subject(process, pid)?;
        if !target.threads().is_empty() {
            return Err(Errno::ESRCH);
        }
        if !credentials::same_owner(process, &target) {
            return Err(Errno::EPERM);
        }
        let runnable = online_cpus().into_iter().any(|cpu| mask.contains(cpu));
        return if runnable { Ok(0) } else { Err(Errno::EINVAL) };
    };
    if !credentials::same_owner(process, thread.process()) {
        return Err(Errno::EPERM);
    }
    let Some(task) = attributes::task_of(&thread) else {
        return Err(Errno::ESRCH);
    };
    match sched::set_affinity(&task, &mask) {
        Ok(_) => Ok(0),
        Err(sched::AffinityRefusal::Empty) => Err(Errno::EINVAL),
        // A Linux thread always runs on a task with a thread; a task with
        // none is the kernel's, and refused (L.sched.96).
        Err(sched::AffinityRefusal::KernelThread) => Err(Errno::EINVAL),
    }
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

/// `SCHED_ATTR_SIZE_VER0`: `struct sched_attr` up to `sched_period`.
const SCHED_ATTR_SIZE_VER0: usize = 48;
/// `SCHED_ATTR_SIZE_VER1`: with the utilisation clamps, the whole structure
/// as this kernel knows it.
const SCHED_ATTR_SIZE_VER1: usize = 56;
/// Linux's `PAGE_SIZE` bound on a `sched_attr` a caller may pass.
const SCHED_ATTR_SIZE_MAX: usize = 4096;
/// `SCHED_FLAG_KEEP_POLICY`: leave the policy as it is.
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
/// `SCHED_FLAG_KEEP_PARAMS`: leave the nice value and the slice as they are.
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
/// The longest `sched_runtime` a fair task may ask for, 100 ms: Linux's
/// ceiling in `__setparam_fair`. What the queue then grants is never longer
/// than its own slice (`ferrix_sched::effective_request`).
const MAX_REQUEST_NS: u64 = 100_000_000;

/// `sched_setattr`: the nice value and the slice request of a fair thread
/// (`docs/GAME-MODE-SCHED.md` §2.2, F7; L.sched.109).
///
/// Only `SCHED_OTHER` at priority zero, as [`sys_sched_setscheduler`]
/// accepts; every other policy, `SCHED_FLAG_RESET_ON_FORK`, the reclaim and
/// overrun flags, the utilisation clamps and any unknown flag are `EINVAL`.
/// `sched_runtime` is zero for the queue's slice, or clamped to 0.1 to
/// 100 ms as Linux 7.0's `__setparam_fair` clamps it. The structure is read
/// as `sched_copy_attr` reads it: a size of zero is the first version's, a
/// size below it or above a page is `E2BIG` with this kernel's size written
/// back, and so is a larger structure whose unknown tail is not zero.
/// Permission is `setpriority`'s, except that a nice value lowered without
/// privilege is `EPERM` here, as `sched_setattr`'s `can_nice` answers.
pub(crate) fn sys_sched_setattr(
    process: &Process,
    pid: i32,
    at: u64,
    flags: u64,
) -> Result<usize, Errno> {
    if at == 0 || pid < 0 || flags != 0 {
        return Err(Errno::EINVAL);
    }
    let attr = read_sched_attr(process, at)?;
    let keep_policy = attr.flags & SCHED_FLAG_KEEP_POLICY != 0;
    let keep_params = attr.flags & SCHED_FLAG_KEEP_PARAMS != 0;
    if attr.flags & !(SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS) != 0
        || (!keep_policy && attr.policy != SCHED_OTHER as u32)
        || attr.priority != 0
    {
        return Err(Errno::EINVAL);
    }
    let thread = attributes::thread_named(process, pid).ok_or(Errno::ESRCH)?;
    if !credentials::same_owner(process, thread.process()) {
        return Err(Errno::EPERM);
    }
    if keep_params {
        return Ok(0);
    }
    let nice = attr.nice.clamp(-20, 19);
    if nice < thread.nice() && credentials::require_privilege(process).is_err() {
        return Err(Errno::EPERM);
    }
    let request = match attr.runtime {
        0 => 0,
        asked => asked.clamp(ferrix_sched::MIN_REQUEST_NS, MAX_REQUEST_NS),
    };
    thread.set_nice(nice);
    if let Some(task) = attributes::task_of(&thread) {
        attributes::apply_nice(thread.nice(), &task);
    }
    attributes::set_slice_request(&thread, request);
    Ok(0)
}

/// `sched_getattr`: the policy, the nice value and the slice request of a
/// fair thread, written as `sched_attr_copy_to_user` writes them: as much of
/// the structure as both sides know, with `size` saying how much that was.
/// `sched_runtime` is the request the thread's task has, or with none the
/// slice a task alone on its queue is given; Linux answers its own base
/// slice there.
pub(crate) fn sys_sched_getattr(
    process: &Process,
    pid: i32,
    at: u64,
    size: u32,
    flags: u64,
) -> Result<usize, Errno> {
    let size = size as usize;
    if at == 0
        || pid < 0
        || flags != 0
        || !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_SIZE_MAX).contains(&size)
    {
        return Err(Errno::EINVAL);
    }
    let thread = attributes::thread_named(process, pid).ok_or(Errno::ESRCH)?;
    // The task's own record, which is what its queue is told, not the
    // thread's: so a task made without its thread's request reads none
    // (L.sched.109's check).
    let asked =
        attributes::task_of(&thread).map_or(thread.slice_request(), |task| task.request_ns());
    let runtime = match asked {
        0 => sched::BASE_SLICE_NS,
        asked => asked,
    };
    let written = size.min(SCHED_ATTR_SIZE_VER1);
    let mut attr = [0_u8; SCHED_ATTR_SIZE_VER1];
    put(&mut attr, 0, &(written as u32).to_ne_bytes());
    put(&mut attr, 4, &(SCHED_OTHER as u32).to_ne_bytes());
    put(&mut attr, 16, &thread.nice().to_ne_bytes());
    put(&mut attr, 24, &runtime.to_ne_bytes());
    let out = attr.get(..written).ok_or(Errno::EINVAL)?;
    uaccess::copy_to_user(process.space(), at, out).map_err(|_| Errno::EFAULT)?;
    Ok(0)
}

/// Write `bytes` into `buffer` at `offset`, where they always fit.
fn put(buffer: &mut [u8], offset: usize, bytes: &[u8]) {
    if let Some(field) = buffer.get_mut(offset..offset + bytes.len()) {
        field.copy_from_slice(bytes);
    }
}

/// The fields of a `struct sched_attr` this kernel reads.
struct SchedAttr {
    /// `sched_policy`.
    policy: u32,
    /// `sched_flags`.
    flags: u64,
    /// `sched_nice`.
    nice: i32,
    /// `sched_priority`.
    priority: u32,
    /// `sched_runtime`, in nanoseconds.
    runtime: u64,
}

/// Read a `struct sched_attr` from `at` as Linux's `sched_copy_attr` does.
fn read_sched_attr(process: &Process, at: u64) -> Result<SchedAttr, Errno> {
    let too_big = || {
        let _ = uaccess::put_u32(process.space(), at, SCHED_ATTR_SIZE_VER1 as u32);
        Errno::E2BIG
    };
    let size = match uaccess::get_u32(process.space(), at)? as usize {
        0 => SCHED_ATTR_SIZE_VER0,
        size => size,
    };
    if !(SCHED_ATTR_SIZE_VER0..=SCHED_ATTR_SIZE_MAX).contains(&size) {
        return Err(too_big());
    }
    let mut raw = [0_u8; SCHED_ATTR_SIZE_VER1];
    let known = size.min(SCHED_ATTR_SIZE_VER1);
    let head = raw.get_mut(..known).ok_or(Errno::EINVAL)?;
    uaccess::copy_from_user(process.space(), at, head).map_err(|_| Errno::EFAULT)?;
    // `copy_struct_from_user`: a tail this kernel does not know must be zero.
    if size > SCHED_ATTR_SIZE_VER1 {
        let mut tail = vec![0_u8; size - SCHED_ATTR_SIZE_VER1];
        uaccess::copy_from_user(process.space(), at + SCHED_ATTR_SIZE_VER1 as u64, &mut tail)
            .map_err(|_| Errno::EFAULT)?;
        if tail.iter().any(|&byte| byte != 0) {
            return Err(too_big());
        }
    }
    let word = |offset: usize| {
        raw.get(offset..offset + 4)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .unwrap_or_default()
    };
    let long = |offset: usize| {
        raw.get(offset..offset + 8)
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .unwrap_or_default()
    };
    Ok(SchedAttr {
        policy: u32::from_ne_bytes(word(4)),
        flags: u64::from_ne_bytes(long(8)),
        nice: i32::from_ne_bytes(word(16)),
        priority: u32::from_ne_bytes(word(20)),
        runtime: u64::from_ne_bytes(long(24)),
    })
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
