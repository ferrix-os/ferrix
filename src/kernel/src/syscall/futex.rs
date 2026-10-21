//! `futex`: sleeping on a word of user memory, and being woken for it.
//!
//! A libc's locks, condition variables and `pthread_join` are a user-space
//! compare-and-swap with this call behind it for the slow path. busybox
//! reaches it through glibc even with one thread -- `system`, `popen` and
//! stdio's locks all lock, and a lock that sees a waiter bit calls in -- so a
//! missing answer is a hang rather than an error.
//!
//! # Linux's arguments over the core's table
//!
//! The table, its keys, the lost-wake rule, wake, requeue and the wait on
//! many words are the core's, `user::address_wait` (K5,
//! `docs/microkernel/K5.md`). The native calls `address_wait` and its family
//! are answered from the same table: one table, not two. This file decodes
//! Linux's arguments and answers Linux's way: the operations and their
//! flags, a relative or absolute `timespec` of either width,
//! `FUTEX_CLOCK_REALTIME` turned into a counter deadline at the call, a wake
//! count of zero or less waking one, a signal answered with a restart code,
//! and `futex_waitv`'s vector. A call gets a shared key when it does not say
//! `FUTEX_PRIVATE_FLAG` and a shared region holds its word, and a private
//! key otherwise, which is also Linux's answer for a private mapping.
//!
//! **One deviation from Linux, on purpose.** Once a vector waiter is woken
//! through one word, its entries on the others stay listed until it runs and
//! takes them off. Linux's `futex_wake` counts such a stale entry as a waiter
//! woken. Here a wake takes it off without counting it and goes on to the
//! next, so it may rouse one real waiter more than Linux would and report a
//! count smaller by the stale entries. For every wait but `futex_waitv`
//! nothing changes.
//!
//! A waiter is charged to its process's job, as a native one is: a wait that
//! the job's memory limit refuses answers `ENOMEM`. seccomp sees every number
//! before it is dispatched, 449 among them.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fallible;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, FUTEX_32, FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE,
    FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAITV_MAX,
    FUTEX_WAITV_SIZE, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX2_PRIVATE,
};

use crate::object::process::{Host, Process};
use crate::syscall::time::TimeWidth;
use crate::syscall::uaccess;
use crate::user::address_wait::{self, Key, MATCH_ANY, Place, WaitError, Word};
use crate::user::space::AddressSpace;

/// The errno a Linux caller sees for what the core's table answered. An
/// interrupted wait answers a restart code, not `EINTR`: a futex wait
/// restarts under `SA_RESTART`, which is how glibc's and musl's condition
/// variables survive a handled signal. The way back settles it.
fn errno(error: WaitError) -> Errno {
    match error {
        WaitError::Changed => Errno::EAGAIN,
        WaitError::Fault => Errno::EFAULT,
        WaitError::NoMemory => Errno::ENOMEM,
        WaitError::Interrupted => Errno::ERESTARTSYS,
        WaitError::TimedOut => Errno::ETIMEDOUT,
    }
}

/// A wake's count as Linux reads it: zero or less still wakes one, since
/// Linux's `futex_wake` counts a waiter before comparing against the limit.
fn wake_limit(count: i32) -> usize {
    usize::try_from(count.max(1)).unwrap_or(1)
}

/// `futex` and `futex_time64`, which differ only in `width`: the size of the
/// `timespec` the timeout argument points at.
///
/// # Errors
///
/// `EAGAIN` if a wait's or a compare-requeue's word no longer holds the value
/// given; `ETIMEDOUT` if a wait's timeout passes; `ERESTARTSYS` if the caller
/// has a signal to take or is ended while it waits; `EINVAL` for a misaligned
/// word, a zero bitset, a negative count or a malformed timeout; `EFAULT` for
/// a word or timeout that cannot be read; `ENOMEM` when the job's memory
/// refuses the waiter; `ENOSYS` for an operation not implemented here, the
/// priority-inheritance ones and `FUTEX_WAKE_OP` among them, and for
/// `FUTEX_CLOCK_REALTIME` on an operation that has no timeout.
pub(crate) fn sys_futex(
    process: &dyn Host,
    a: &[u64; 6],
    width: TimeWidth,
) -> Result<usize, Errno> {
    let [address, op, value, timeout, address2, value3] = *a;
    let op = op as u32;
    // The ABI's `u32 val` and `u32 val3`; the upper half of a 64-bit register
    // is whatever the caller left there.
    let value = value as u32;
    let value3 = value3 as u32;
    let command = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    let shared = op & FUTEX_PRIVATE_FLAG == 0;
    if op & FUTEX_CLOCK_REALTIME != 0 && !matches!(command, FUTEX_WAIT | FUTEX_WAIT_BITSET) {
        return Err(Errno::ENOSYS);
    }
    match command {
        FUTEX_WAIT => {
            let deadline = if timeout == 0 {
                None
            } else {
                let relative = read_timespec(process, timeout, width)?;
                Some(crate::timer::now_nanos().saturating_add(relative))
            };
            wait(process, address, shared, value, MATCH_ANY, deadline)
        }
        FUTEX_WAIT_BITSET => {
            if value3 == 0 {
                return Err(Errno::EINVAL);
            }
            // Absolute, on `CLOCK_MONOTONIC` -- the counter -- or with
            // `FUTEX_CLOCK_REALTIME` on the real-time clock, which is the
            // counter plus its offset, as `clock_nanosleep` turns one. Read
            // as a counter deadline, a real-time one lay decades ahead, and
            // glibc's `pthread_cond_timedwait`, `sem_timedwait` and
            // `pthread_mutex_timedlock` never timed out (ferrix-ea,
            // 2026-09-26).
            let deadline = if timeout == 0 {
                None
            } else {
                let absolute = read_timespec(process, timeout, width)?;
                Some(if op & FUTEX_CLOCK_REALTIME != 0 {
                    counter_deadline(absolute)
                } else {
                    absolute
                })
            };
            wait(process, address, shared, value, value3, deadline)
        }
        FUTEX_WAKE => {
            let key = key(process.core(), address, shared)?;
            Ok(address_wait::wake(
                &key,
                wake_limit(value as i32),
                MATCH_ANY,
            ))
        }
        FUTEX_WAKE_BITSET => {
            if value3 == 0 {
                return Err(Errno::EINVAL);
            }
            let key = key(process.core(), address, shared)?;
            Ok(address_wait::wake(&key, wake_limit(value as i32), value3))
        }
        // The fourth argument is a count, not a pointer, for these two: Linux
        // reads it as `val2`, the register narrowed to an `int`.
        FUTEX_REQUEUE => requeue(
            process,
            (address, address2),
            shared,
            (value as i32, timeout as i32),
            None,
        ),
        FUTEX_CMP_REQUEUE => requeue(
            process,
            (address, address2),
            shared,
            (value as i32, timeout as i32),
            Some(value3),
        ),
        _ => Err(Errno::ENOSYS),
    }
}

/// A real-time deadline as a counter one: less the real-time offset, at the
/// call. A later step of the real-time clock is not followed.
fn counter_deadline(absolute: u64) -> u64 {
    let counter = i128::from(absolute) - i128::from(crate::syscall::time::realtime_offset());
    u64::try_from(counter.max(0)).unwrap_or(u64::MAX)
}

/// `futex_waitv`: sleep on up to [`FUTEX_WAITV_MAX`] words until one is
/// woken, and answer its index. `width` is the `__kernel_timespec` the
/// timeout points at, in the caller's ABI ([`TimeWidth::CompatWide`] for an
/// i386 caller, whose word addresses must also lie below 4 GiB).
///
/// The arguments are `waiters`, `nr_futexes`, `flags`, `timeout` and
/// `clockid`. The timeout, if any, is absolute on `CLOCK_MONOTONIC` or
/// `CLOCK_REALTIME`.
///
/// # Errors
///
/// `EINVAL` for flags, no words or more than [`FUTEX_WAITV_MAX`], a null
/// vector, another clock with a timeout, a malformed timeout, an element
/// whose flags are not `FUTEX_32` with or without `FUTEX2_PRIVATE`, whose
/// reserved word is not zero, whose value does not fit 32 bits or whose
/// address is misaligned; `EFAULT` for a vector, timeout or word that
/// cannot be read, an i386 caller's word at or above 4 GiB among them;
/// `ENOMEM` when the vector cannot be copied or the waiter is refused; and
/// a wait's answers.
pub(crate) fn sys_futex_waitv(
    process: &dyn Host,
    a: &[u64; 6],
    width: TimeWidth,
) -> Result<usize, Errno> {
    let [waiters, count, flags, timeout, clock, _] = *a;
    // `unsigned int nr_futexes`, `unsigned int flags` and `clockid_t`: the
    // upper half of a 64-bit register is whatever the caller left there.
    let (count, flags, clock) = (count as u32 as usize, flags as u32, clock as u32);
    if flags != 0 || count == 0 || count > FUTEX_WAITV_MAX || waiters == 0 {
        return Err(Errno::EINVAL);
    }
    let deadline = if timeout == 0 {
        None
    } else {
        if clock != CLOCK_MONOTONIC && clock != CLOCK_REALTIME {
            return Err(Errno::EINVAL);
        }
        let absolute = read_timespec(process, timeout, width)?;
        Some(if clock == CLOCK_REALTIME {
            counter_deadline(absolute)
        } else {
            absolute
        })
    };
    let compat = width == TimeWidth::CompatWide;
    let mut vector =
        fallible::try_with_capacity::<u8>(count * FUTEX_WAITV_SIZE).map_err(|_| Errno::ENOMEM)?;
    vector.resize(count * FUTEX_WAITV_SIZE, 0);
    uaccess::copy_from_user(process.core().space(), waiters, &mut vector)
        .map_err(|_| Errno::EFAULT)?;
    let mut words: Vec<Word> = fallible::try_with_capacity(count).map_err(|_| Errno::ENOMEM)?;
    for element in vector.chunks_exact(FUTEX_WAITV_SIZE) {
        let field = |at: usize| -> u64 {
            let mut bytes = [0_u8; 8];
            if let Some(source) = element.get(at..at + 8) {
                bytes.copy_from_slice(source);
            }
            u64::from_le_bytes(bytes)
        };
        let (value, address, tail) = (field(0), field(8), field(16));
        let (element_flags, reserved) = (tail as u32, (tail >> 32) as u32);
        if (element_flags != FUTEX_32 && element_flags != FUTEX_32 | FUTEX2_PRIVATE)
            || reserved != 0
        {
            return Err(Errno::EINVAL);
        }
        let expected = u32::try_from(value).map_err(|_| Errno::EINVAL)?;
        if !address.is_multiple_of(4) {
            return Err(Errno::EINVAL);
        }
        if compat && address > u64::from(u32::MAX) {
            return Err(Errno::EFAULT);
        }
        let shared = element_flags & FUTEX2_PRIVATE == 0;
        words.push(Word {
            place: Place::User(address),
            expected,
            key: key(process.core(), address, shared)?,
            bitset: MATCH_ANY,
        });
    }
    // The keys go after the copy; nothing is listed yet.
    drop(vector);
    wait_on(process, &words, deadline)
}

/// The key for a futex word, which must be aligned as a 32-bit word is:
/// shared if `shared` and a shared region holds it.
fn key(process: &Process, address: u64, shared: bool) -> Result<Key, Errno> {
    if !address.is_multiple_of(4) {
        return Err(Errno::EINVAL);
    }
    Ok(Key::new(process.space(), address, shared))
}

/// The core's wait on `words` for a Linux caller: interrupted when it has a
/// signal to take or is ending, charged to its process's job.
fn wait_on(process: &dyn Host, words: &[Word], deadline: Option<u64>) -> Result<usize, Errno> {
    let core = process.core();
    address_wait::wait_on(
        core.space(),
        words,
        deadline,
        &|| process.wait_interrupted(),
        core.quota_slot(),
    )
    .map_err(errno)
}

/// Sleep on `address` if it holds `expected`, until a wake whose bitset
/// shares a bit with `bitset`, the caller is ended, or `deadline` passes.
/// `shared` is whether the call left out `FUTEX_PRIVATE_FLAG`.
fn wait(
    process: &dyn Host,
    address: u64,
    shared: bool,
    expected: u32,
    bitset: u32,
    deadline: Option<u64>,
) -> Result<usize, Errno> {
    let word = Word {
        place: Place::User(address),
        expected,
        key: key(process.core(), address, shared)?,
        bitset,
    };
    wait_on(process, core::slice::from_ref(&word), deadline).map(|_| 0)
}

/// `FUTEX_REQUEUE` and, with `expected`, `FUTEX_CMP_REQUEUE`: rouse up to
/// `wake_count` waiters on `from` and move up to `move_count` more onto `to`.
/// Answers how many were roused or moved.
fn requeue(
    process: &dyn Host,
    (from, to): (u64, u64),
    shared: bool,
    (wake_count, move_count): (i32, i32),
    expected: Option<u32>,
) -> Result<usize, Errno> {
    let (Ok(wake_count), Ok(move_count)) =
        (usize::try_from(wake_count), usize::try_from(move_count))
    else {
        return Err(Errno::EINVAL);
    };
    let core = process.core();
    let source = key(core, from, shared)?;
    let target = key(core, to, shared)?;
    let compare = expected.map(|expected| Word {
        place: Place::User(from),
        expected,
        key: source.clone(),
        bitset: MATCH_ANY,
    });
    address_wait::requeue(
        core.space(),
        &source,
        &target,
        (wake_count, move_count),
        compare.as_ref(),
    )
    .map_err(errno)
}

/// Rouse up to `count` waiters on `address` in `space`: what a process's end
/// does for the address `CLONE_CHILD_CLEARTID` or `set_tid_address` gave it.
/// Keyed as a shared wake, as Linux wakes it, so a waiter that left out
/// `FUTEX_PRIVATE_FLAG` on a word in shared memory is found too. Takes the
/// space's lock.
pub(crate) fn wake_address(space: &Arc<AddressSpace>, address: u64, count: i32) -> usize {
    address_wait::wake(
        &Key::new(space, address, true),
        wake_limit(count),
        MATCH_ANY,
    )
}

/// Both keys a waiter on `address` in `space` can have, whether or not it
/// said `FUTEX_PRIVATE_FLAG`: for the self-checks, which watch programs whose
/// flags are theirs to choose.
fn either_key(space: &Arc<AddressSpace>, address: u64) -> [Key; 2] {
    [
        Key::new(space, address, false),
        Key::new(space, address, true),
    ]
}

/// How many waiters are on `address` in `process`'s space right now, keyed
/// either way.
///
/// For the self-check, which has to know its waiter is asleep before it can
/// tell a wake that works from one that arrived first.
pub(crate) fn waiters_on(process: &Process, address: u64) -> usize {
    address_wait::waiters_on(&either_key(process.space(), address))
}

/// The bucket a private wait on `address` in `process`'s space is listed in:
/// for the self-check, which needs two words in different buckets.
pub(crate) fn bucket_of(process: &Process, address: u64) -> usize {
    address_wait::bucket_of(&Key::new(process.space(), address, false))
}

/// The negative controls' and the stale-entry check's hooks, over the core's
/// sabotage hooks: compiled only into a kernel with its self-checks.
#[cfg(ferrix_checks = "on")]
pub(crate) mod sabotage {
    use super::{Errno, Key, Process, address_wait, either_key, key};

    pub(crate) use address_wait::sabotage::StaleEntry;

    /// Take up to `count` waiters off `address`, keyed either way, and report
    /// them woken without rousing them: a wake with the one bug a count
    /// cannot see.
    pub(crate) fn forget_waiters(process: &Process, address: u64, count: i32) -> usize {
        address_wait::sabotage::forget_waiters(
            &either_key(process.space(), address),
            super::wake_limit(count),
        )
    }

    /// List a [`StaleEntry`] on `address` in `process`'s space, keyed as a
    /// private wait is: slot 1 of a sleeper woken through slot 0.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a misaligned word, `ENOMEM` when memory is refused.
    pub(crate) fn list_stale_entry(process: &Process, address: u64) -> Result<StaleEntry, Errno> {
        address_wait::sabotage::list_stale_entry(key(process, address, false)?)
            .map_err(|()| Errno::ENOMEM)
    }

    /// A private requeue of every waiter on `from` to `to` that changes each
    /// entry's key where it lies and leaves it in `from`'s bucket.
    pub(crate) fn requeue_without_moving(process: &Process, from: u64, to: u64) -> usize {
        address_wait::sabotage::requeue_without_moving(
            &Key::new(process.space(), from, false),
            &Key::new(process.space(), to, false),
        )
    }
}

/// Read a `struct timespec` of `width` as nanoseconds, by `ppoll`'s rules.
fn read_timespec(process: &dyn Host, at: u64, width: TimeWidth) -> Result<u64, Errno> {
    crate::syscall::poll::read_timespec(process.core(), at, width)
}
