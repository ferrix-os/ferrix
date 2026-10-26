//! `futex`: sleeping on a word of user memory, and being woken for it.
//!
//! A libc's locks, condition variables and `pthread_join` are a user-space
//! compare-and-swap with this call behind it for the slow path. busybox
//! reaches it through glibc even with one thread -- `system`, `popen` and
//! stdio's locks all lock, and a lock that sees a waiter bit calls in -- so a
//! missing answer is a hang rather than an error.
//!
//! # What a wait is keyed by
//!
//! Two ways, as Linux keys them. A private key is the address space and the
//! user address: two processes' words at the same address are different
//! words. The space is safe to name by address: every waiter holds its
//! process, and so the space, for as long as it is on the table.
//!
//! A shared key is the object behind a shared mapping and the byte offset of
//! the word in it: the same word however many spaces map it, and at whatever
//! address each maps it. A futex in `MAP_SHARED` memory between the two sides
//! of a `fork` -- a `PTHREAD_PROCESS_SHARED` mutex, condition variable or
//! barrier -- is only one futex because of it; keyed by space, a waker in one
//! process never found a waiter in the other. A call gets a shared key when
//! it does not say `FUTEX_PRIVATE_FLAG` and a shared region holds its word,
//! and a private key otherwise, which is also Linux's answer for a private
//! mapping. The key holds its object, so the object's identity cannot be
//! reused by another while anyone waits on it -- and so a key is never
//! dropped with [`TABLE`] held, since the last reference to an object may
//! free its pages.
//!
//! # The lost wake-up, again
//!
//! `FUTEX_WAIT` promises to sleep only if the word still holds the value the
//! caller saw, and a `FUTEX_WAKE` that follows a change must find every waiter
//! that saw the old value. Both are kept by one lock: the word is read, and
//! the waiter put on the table, while the table's lock is held, and a waker
//! takes the same lock to look. A waker that changed the word before the read
//! makes the read see the change, and one that changed it after finds the
//! waiter listed.
//!
//! The read under the lock never faults. Resolving a fault takes the space's
//! lock and allocates a frame, and once memory is reclaimed it may wait, none
//! of which a spin lock's holder may do. So the word is read first with
//! nothing held, faulting its page in, and read again under the lock only if
//! its page is still there, through [`uaccess::copy_from_user_present`]. With
//! threads the page can go in between -- another thread unmaps it -- and then
//! the lock is let go and the whole read done again. A `SpinLock` stays the
//! right kind for the table: no interrupt handler wakes a futex, and nothing
//! done under it now sleeps. `AddressSpace::fault` asserts that it is never
//! resolved under a lock that disables preemption, which is what names this
//! read if it ever faults under the table again.
//!
//! A waker rouses a waiter while still holding the lock, and a waiter leaving
//! for any reason takes the lock before it returns. So a wake can never land
//! after its waiter has gone on to sleep for something else -- the failure
//! `sched::wait`'s comments describe, a sleep cut short by a wake meant for an
//! earlier one.
//!
//! # Buckets
//!
//! Linux hashes keys into buckets, each with its own lock, and so does this:
//! [`TABLE`] is [`BUCKETS`] of them, and a key's bucket is its hash. Until
//! 2026-09-26 it was one bucket, because nothing had made enough futex calls
//! at once to contend for it. Chrome playing a video did: its renderer made
//! about 4,500 calls a second, every one scanning a list that held all of the
//! browser's hundred and fifty parked threads, and one processor in five was
//! spent spinning for the lock (`bench-chrome-video`, `docs/AUDIO.md` §8).
//! Under KVM a holder's virtual processor can also be descheduled by the
//! host, and every other caller then spins until it runs again.
//!
//! Everything a lock guarded before, a bucket's lock guards now for the keys
//! that hash to it: a waiter is listed, and its word read, under its key's
//! bucket, and a waker looks under the same one. A requeue between two keys
//! takes both buckets' locks, the lower-numbered first, and a waiter records
//! which bucket its entry is in ([`Sleeper::bucket`]), changed only with both
//! locks held. A waiter leaving takes the lock of the bucket it reads there
//! and looks again: if the entry was moved in between, it follows it.
//!
//! # Many words at once: `futex_waitv`
//!
//! `futex_waitv` (449) sleeps on up to [`FUTEX_WAITV_MAX`] words until any
//! one of them is woken, and answers which. Proton's Wine waits through it
//! for its fsync: its objects are words in a POSIX shared-memory file that
//! wineserver and every Wine process map, and a wait for any of several
//! objects is one call. A waiter is one [`Sleeper`] with one entry per word
//! -- its *slot* -- each listed in its own key's bucket, and the sleeper
//! records per slot which bucket that entry is in, so that leaving and a
//! requeue work per entry as they did for the one entry before. Setting up
//! takes one bucket's lock at a time, word by word, and reads each word
//! under its lock as a single wait does; a word that no longer holds its
//! value unlists every slot listed so far and answers `EAGAIN`, unless one of
//! them was woken in between, which is then the answer, as on Linux. Leaving
//! unlists every slot, again one lock at a time. A single `FUTEX_WAIT` is the
//! one-slot case of the same wait and behaves as before.
//!
//! **One deviation from Linux, on purpose.** Once a vector waiter is woken
//! through one word, its entries on the others stay listed until it runs and
//! takes them off. Linux's `futex_wake` counts such a stale entry as a waiter
//! woken, so a wake of one meant for the next real waiter on that word can
//! be spent on a task that is already awake. Here a wake, a requeue, a
//! thread's exit wake and the self-checks' forgetting wake all go through
//! the same [`Sleeper::rouse`], which takes a stale entry off without
//! counting it -- neither as roused nor as moved -- and goes on to the next.
//! A wake therefore never loses a waiter to a stale entry; it may rouse one
//! real waiter more than Linux would and report a count smaller by the stale
//! entries. A one-slot waiter's entry always leaves the table with the wake
//! that rouses it, so for every wait but `futex_waitv` nothing changes.
//!
//! Every entry and the copied vector are fallible allocations: memory
//! refused unlists what was listed and answers `ENOMEM`. A task holds at most
//! [`FUTEX_WAITV_MAX`] entries, and seccomp sees 449 before it is dispatched,
//! as it sees every number.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::fallible;
use crate::sync::SpinLock;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    CLOCK_MONOTONIC, CLOCK_REALTIME, FUTEX_32, FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE,
    FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT, FUTEX_WAIT_BITSET, FUTEX_WAITV_MAX,
    FUTEX_WAITV_SIZE, FUTEX_WAKE, FUTEX_WAKE_BITSET, FUTEX2_PRIVATE,
};

use crate::object::process::{Host, Process};
use crate::sched::{self, Task, WaitQueue};
use crate::syscall::signal::RestartBlock;
use crate::syscall::time::TimeWidth;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;

/// A bitset that matches every waiter: what the plain `WAIT` and `WAKE` use.
const FUTEX_BITSET_MATCH_ANY: u32 = 0xFFFF_FFFF;

/// Which word a waiter sleeps on. See the module documentation.
#[derive(Debug, Clone)]
enum Key {
    /// A word only one address space can name.
    Private {
        /// The address space, by address.
        space: usize,
        /// The word's user address.
        address: u64,
    },
    /// A word in an object that shared regions map.
    Shared {
        /// The object, held so that its address names it alone.
        object: Arc<Vmo>,
        /// The word's byte offset in the object.
        offset: u64,
    },
}

impl PartialEq for Key {
    fn eq(&self, other: &Key) -> bool {
        match (self, other) {
            (
                Key::Private { space, address },
                Key::Private {
                    space: other_space,
                    address: other_address,
                },
            ) => space == other_space && address == other_address,
            (
                Key::Shared { object, offset },
                Key::Shared {
                    object: other_object,
                    offset: other_offset,
                },
            ) => Arc::ptr_eq(object, other_object) && offset == other_offset,
            _ => false,
        }
    }
}

impl Eq for Key {}

impl Key {
    /// The bucket of [`TABLE`] this key's waiters are listed in: the space
    /// or object and the word's place in it, mixed by a Fibonacci hash, whose
    /// top bits are the index.
    fn bucket(&self) -> usize {
        let (base, at) = match self {
            Key::Private { space, address } => (*space as u64, *address),
            Key::Shared { object, offset } => (Arc::as_ptr(object).addr() as u64, *offset),
        };
        let mixed = (base.rotate_left(29) ^ at).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> (u64::BITS - BUCKET_BITS)) as usize
    }

    /// The key for `address` in `space`: shared if `shared` asks for one and
    /// a shared region holds the word, private otherwise. Takes the space's
    /// lock, so never with [`TABLE`] held.
    fn new(space: &Arc<AddressSpace>, address: u64, shared: bool) -> Key {
        if shared && let Some((object, offset)) = space.shared_object_at(address) {
            return Key::Shared { object, offset };
        }
        Key::Private {
            space: Arc::as_ptr(space).addr(),
            address,
        }
    }
}

/// What [`Sleeper::woken`] holds until a wake takes the sleeper.
const NOT_WOKEN: usize = usize::MAX;

/// One task asleep in `FUTEX_WAIT` or `futex_waitv`, as its wakers see it.
#[derive(Debug)]
struct Sleeper {
    /// The task to make runnable. `None` only before the scheduler starts,
    /// when a wait spins on `woken` instead.
    task: Option<Arc<Task>>,
    /// The slot whose wake took it, set once by the first wake to come, or
    /// [`NOT_WOKEN`].
    woken: AtomicUsize,
    /// Per slot, the bucket of [`TABLE`] its entry is in, or was in last.
    /// Changed only by a requeue, holding both the old bucket's lock and the
    /// new one's. Never empty.
    buckets: Vec<AtomicUsize>,
}

impl Sleeper {
    /// Mark it woken through `slot` and make its task runnable, if no wake
    /// took it before: whether this one did. Called with the lock of the
    /// bucket the entry for `slot` was in held. A `false` is a stale entry,
    /// which the caller takes off without counting (the module's
    /// documentation says why).
    fn rouse(&self, slot: usize) -> bool {
        if self
            .woken
            .compare_exchange(NOT_WOKEN, slot, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        if let Some(task) = &self.task {
            sched::wake(task);
        }
        true
    }

    /// Whether a wake has taken it.
    fn is_woken(&self) -> bool {
        self.woken.load(Ordering::Acquire) != NOT_WOKEN
    }

    /// Where the bucket of `slot`'s entry is recorded; `None` only for a
    /// slot past the last, which no caller asks for.
    fn bucket_of(&self, slot: usize) -> Option<&AtomicUsize> {
        self.buckets.get(slot)
    }
}

/// A waiter on the table.
#[derive(Debug)]
struct Entry {
    /// What it waits on; changed in place by a requeue.
    key: Key,
    /// Which wakes it answers to: `FUTEX_WAKE_BITSET` rouses it only if the
    /// two share a bit.
    bitset: u32,
    /// Who it is.
    sleeper: Arc<Sleeper>,
    /// Which of the sleeper's words this entry is: 0 for a single wait, the
    /// index into the vector for `futex_waitv`.
    slot: usize,
}

/// What [`rouse`] does to each matching entry it takes off the table; it
/// answers whether the entry counts.
type With = fn(&Entry) -> bool;

/// [`With`] for a real wake.
fn rouse_entry(entry: &Entry) -> bool {
    entry.sleeper.rouse(entry.slot)
}

/// How long a futex wait sleeps between looks of its own: a second, the
/// recheck `fs::wake` gives a wait whose every queue is trusted, since every
/// way a futex wait ends wakes it by name.
const TRUSTED_RECHECK_NANOS: u64 = 1_000_000_000;

/// Bits of a key's hash that pick its bucket.
const BUCKET_BITS: u32 = 8;

/// Buckets in [`TABLE`].
const BUCKETS: usize = 1 << BUCKET_BITS;

/// Every waiter, by its key's bucket ([`Key::bucket`]), each bucket oldest
/// first, so that a wake rouses in arrival order as Linux's does.
static TABLE: [SpinLock<Vec<Entry>>; BUCKETS] = [const { SpinLock::new(Vec::new()) }; BUCKETS];

/// Bucket `index` of [`TABLE`]. [`Key::bucket`] is the top [`BUCKET_BITS`]
/// of a hash, so every index is in range and the first bucket is never
/// answered in another's place; it is there because a lookup has to answer
/// something.
fn bucket_lock(index: usize) -> &'static SpinLock<Vec<Entry>> {
    TABLE.get(index).unwrap_or(&TABLE[0])
}

/// What waiters block on. Nobody wakes it as a whole: a wake rouses its
/// waiters' tasks one by one, and this is only the blocking mechanism.
static SLEEP: WaitQueue = WaitQueue::new();

/// `futex` and `futex_time64`, which differ only in `width`: the size of the
/// `timespec` the timeout argument points at.
///
/// # Errors
///
/// `EAGAIN` if a wait's or a compare-requeue's word no longer holds the value
/// given; `ETIMEDOUT` if a wait's timeout passes; `EINTR` if the caller is
/// ended while it waits, or if a handler interrupts a wait with a timeout
/// (whatever `SA_RESTART` says, as on Linux); `EINVAL` for a misaligned word, a zero bitset, a
/// negative count or a malformed timeout; `EFAULT` for a word or timeout that
/// cannot be read; `ENOSYS` for an operation not implemented here, the
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
            wait(
                process,
                address,
                shared,
                value,
                FUTEX_BITSET_MATCH_ANY,
                deadline,
            )
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
                    let counter =
                        i128::from(absolute) - i128::from(crate::syscall::time::realtime_offset());
                    u64::try_from(counter.max(0)).unwrap_or(u64::MAX)
                } else {
                    // In the caller's time namespace (Linux's
                    // `timens_ktime_to_host(CLOCK_MONOTONIC, ..)`).
                    crate::syscall::userns::acting().map_or(absolute, |caller| {
                        crate::syscall::time::host_from(&caller, CLOCK_MONOTONIC, absolute)
                    })
                })
            };
            wait(process, address, shared, value, value3, deadline)
        }
        FUTEX_WAKE => wake(
            process,
            address,
            shared,
            value as i32,
            FUTEX_BITSET_MATCH_ANY,
        ),
        FUTEX_WAKE_BITSET => {
            if value3 == 0 {
                return Err(Errno::EINVAL);
            }
            wake(process, address, shared, value as i32, value3)
        }
        // The fourth argument is a count, not a pointer, for these two: Linux
        // reads it as `val2`, the register narrowed to an `int`.
        FUTEX_REQUEUE => requeue(
            process,
            (address, address2),
            shared,
            value as i32,
            timeout as i32,
            None,
        ),
        FUTEX_CMP_REQUEUE => requeue(
            process,
            (address, address2),
            shared,
            value as i32,
            timeout as i32,
            Some(value3),
        ),
        _ => Err(Errno::ENOSYS),
    }
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
/// `ENOMEM` when the vector cannot be copied; and [`wait_on`]'s answers.
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
            let counter =
                i128::from(absolute) - i128::from(crate::syscall::time::realtime_offset());
            u64::try_from(counter.max(0)).unwrap_or(u64::MAX)
        } else {
            // In the caller's time namespace, as `FUTEX_WAIT_BITSET`'s: Linux
            // v6.12's `futex2_setup_timeout` goes through
            // `futex_init_timeout(FUTEX_WAIT_BITSET, ..)`, which takes it to the
            // host with `timens_ktime_to_host` (`kernel/futex/syscalls.c`).
            crate::syscall::userns::acting().map_or(absolute, |caller| {
                crate::syscall::time::host_from(&caller, CLOCK_MONOTONIC, absolute)
            })
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
            address,
            expected,
            key: key(process.core(), address, shared)?,
            bitset: FUTEX_BITSET_MATCH_ANY,
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

/// Read the futex word at `address`, faulting its page in. Never with
/// [`TABLE`] held.
fn read_word(process: &Process, address: u64) -> Result<u32, Errno> {
    let mut bytes = [0_u8; 4];
    uaccess::copy_from_user(process.space(), address, &mut bytes).map_err(|_| Errno::EFAULT)?;
    Ok(u32::from_le_bytes(bytes))
}

/// Read the futex word at `address` with [`TABLE`] held: `None` if its page is
/// not there to be read without a fault, which the caller answers by letting
/// go of the table and faulting it in with [`read_word`].
///
/// The word is aligned and four bytes long, so it lies in one page and the read
/// is all or nothing.
fn read_present_word(process: &Process, address: u64) -> Result<Option<u32>, Errno> {
    let mut bytes = [0_u8; 4];
    let read = uaccess::copy_from_user_present(process.space(), address, &mut bytes)
        .map_err(|_| Errno::EFAULT)?;
    Ok(read.then(|| u32::from_le_bytes(bytes)))
}

/// Block until `sleeper` is woken, the caller has a signal to take -- which
/// ends the wait with `EINTR`, as it ends every wait -- or `deadline` passes.
///
/// Trusting its wakes, as `poll` trusts a file's queues: a wake rouses the
/// sleeper's task by name, a signal wakes the thread it is for, and an ending
/// or stopping process wakes all of its own. The few milliseconds a wait
/// queue sleeps between looks of its own were every blocked thread woken two
/// hundred times a second for nothing -- a browser's hundred and fifty of
/// them, four processors kept busy doing it.
fn sleep(sleeper: &Sleeper, process: &dyn Host, deadline: u64) {
    let _ = WaitQueue::wait_on_any(
        &[&SLEEP],
        || sleeper.is_woken() || process.wait_interrupted(),
        deadline,
        TRUSTED_RECHECK_NANOS,
    );
}

/// One word a wait sleeps on: its address, the value it must still hold,
/// its key and the bitset its wakes must share a bit with.
struct Word {
    /// The word's user address, aligned.
    address: u64,
    /// What it must hold for the wait to sleep.
    expected: u32,
    /// What it is keyed by.
    key: Key,
    /// Which wakes rouse it.
    bitset: u32,
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
    let key = key(process.core(), address, shared)?;
    let mut words = fallible::try_with_capacity(1).map_err(|_| Errno::ENOMEM)?;
    words.push(Word {
        address,
        expected,
        key,
        bitset,
    });
    match wait_on(process, &words, deadline) {
        Ok(_) => Ok(0),
        // With a timeout, a signal leaves a restart block, as Linux's
        // `futex_wait` does: a handler makes the call `EINTR` whatever
        // `SA_RESTART` says, and a stop or an ignored signal resumes it
        // through `restart_syscall` to the same deadline. Restarted from the
        // start, a relative `FUTEX_WAIT` waited its whole timeout again
        // after every handler, and a caller polling with short timeouts
        // under a stream of signals could wait for ever.
        Err(Errno::ERESTARTSYS) => match (deadline, crate::syscall::thread::current()) {
            (Some(deadline), Some(thread)) => {
                thread.with_own_signals(|signals| {
                    signals.set_restart_block(RestartBlock::Futex {
                        address,
                        shared,
                        expected,
                        bitset,
                        deadline,
                    });
                });
                Err(Errno::ERESTART_RESTARTBLOCK)
            }
            _ => Err(Errno::ERESTARTSYS),
        },
        Err(error) => Err(error),
    }
}

/// `restart_syscall` for a timed futex wait a signal interrupted with no
/// handler run: the same wait, to the same absolute deadline.
pub(crate) fn resume_wait(
    thread: &crate::syscall::thread::Thread,
    address: u64,
    shared: bool,
    expected: u32,
    bitset: u32,
    deadline: u64,
) -> Result<usize, Errno> {
    if crate::timer::now_nanos() >= deadline {
        return Err(Errno::ETIMEDOUT);
    }
    wait(
        &**thread.process(),
        address,
        shared,
        expected,
        bitset,
        Some(deadline),
    )
}

/// Sleep on every one of `words` that still holds its value, until a wake
/// of any of them, the caller is ended, or `deadline` passes; answer the
/// index of the word whose wake took it.
///
/// **A wait that ends for none of those goes round again**, reading the words
/// afresh, as Linux's `futex_wait` does after a spurious wakeup. What ended it
/// can be gone by the time the answer is chosen: a `SIGSTOP` pending for the
/// process ends the wait, and another thread may take it before this one looks
/// again -- dequeued, and the process not yet marked stopped -- or the stop may
/// be over, continued while this thread had not yet run. Answered as a
/// timeout, a wait with none returned `ETIMEDOUT` from a stop: FX-0701's "a
/// `FUTEX_WAIT` stopped and continued returned instead of being restarted".
///
/// # Errors
///
/// `EAGAIN` if a word no longer holds its value when it is listed and no
/// word listed before it was woken in between; `EFAULT` for a word that
/// cannot be read; `ENOMEM` if an entry cannot be listed; `ERESTARTSYS` for
/// a signal; `ETIMEDOUT` once `deadline` passes.
fn wait_on(process: &dyn Host, words: &[Word], deadline: Option<u64>) -> Result<usize, Errno> {
    let mut buckets = fallible::try_with_capacity(words.len()).map_err(|_| Errno::ENOMEM)?;
    buckets.extend(words.iter().map(|word| AtomicUsize::new(word.key.bucket())));
    let sleeper = fallible::try_arc(Sleeper {
        task: sched::current(),
        woken: AtomicUsize::new(NOT_WOKEN),
        buckets,
    })
    .map_err(|_| Errno::ENOMEM)?;
    let deadline = deadline.unwrap_or(u64::MAX);
    loop {
        for (slot, word) in words.iter().enumerate() {
            if let Err(error) = list(process, &sleeper, slot, word) {
                // Off the table, every slot listed before this one; one woken
                // in between is the answer, as on Linux.
                for listed in 0..slot {
                    drop(unlist(&sleeper, listed));
                }
                return match sleeper.woken.load(Ordering::Acquire) {
                    NOT_WOKEN => Err(error),
                    woken => Ok(woken),
                };
            }
        }

        sleep(&sleeper, process, deadline);

        // Off the table however it left, and under the lock, so that a waker
        // which found it has finished rousing it before this returns. The
        // entries go after each lock, with the keys they hold.
        for slot in 0..words.len() {
            drop(unlist(&sleeper, slot));
        }
        // Woken wins over the other two: a wake that took this waiter off the
        // table counted it, and reporting a timeout would lose that wake for
        // whoever the waker meant it for.
        let woken = sleeper.woken.load(Ordering::Acquire);
        if woken != NOT_WOKEN {
            return Ok(woken);
        }
        if process.wait_interrupted() {
            // A restart code, not `EINTR`: a futex wait restarts under
            // `SA_RESTART`, which is how glibc's and musl's condition
            // variables survive a handled signal. The way back settles it.
            return Err(Errno::ERESTARTSYS);
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(Errno::ETIMEDOUT);
        }
    }
}

/// List `sleeper`'s entry for `slot` on `word`, if the word still holds its
/// value: read first with nothing held, faulting its page in, then again
/// under its bucket's lock without a fault, round again if the page went in
/// between.
///
/// # Errors
///
/// `EAGAIN` if the word holds another value, `EFAULT` if it cannot be read,
/// `ENOMEM` if the bucket cannot grow.
fn list(process: &dyn Host, sleeper: &Arc<Sleeper>, slot: usize, word: &Word) -> Result<(), Errno> {
    let bucket = word.key.bucket();
    loop {
        let _ = read_word(process.core(), word.address)?;
        let mut table = bucket_lock(bucket).lock();
        match read_present_word(process.core(), word.address)? {
            None => {}
            Some(value) if value != word.expected => return Err(Errno::EAGAIN),
            Some(_) => {
                if let Some(recorded) = sleeper.bucket_of(slot) {
                    recorded.store(bucket, Ordering::Relaxed);
                }
                return fallible::try_push(
                    &mut table,
                    Entry {
                        key: word.key.clone(),
                        bitset: word.bitset,
                        sleeper: Arc::clone(sleeper),
                        slot,
                    },
                )
                .map_err(|_| Errno::ENOMEM);
            }
        }
    }
}

/// Take `sleeper`'s entry for `slot` off the table if it is still there,
/// holding the lock of the bucket it is in. A requeue may move it while this
/// waits for that lock, so the bucket is read again once the lock is held,
/// and if it changed, the lock of the one it names is taken instead.
fn unlist(sleeper: &Arc<Sleeper>, slot: usize) -> Option<Entry> {
    let recorded = sleeper.bucket_of(slot)?;
    loop {
        let bucket = recorded.load(Ordering::Acquire);
        let mut table = bucket_lock(bucket).lock();
        if recorded.load(Ordering::Acquire) != bucket {
            continue;
        }
        let mine = table
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.sleeper, sleeper) && entry.slot == slot)
            .map(|index| table.remove(index));
        drop(table);
        return mine;
    }
}

/// Rouse up to `count` waiters on `address` whose bitset shares a bit with
/// `bitset`, oldest first, and report how many.
fn wake(
    process: &dyn Host,
    address: u64,
    shared: bool,
    count: i32,
    bitset: u32,
) -> Result<usize, Errno> {
    let key = key(process.core(), address, shared)?;
    Ok(rouse(&[key], count, bitset, rouse_entry))
}

/// Take up to `count` waiters on any of `keys` whose bitset shares a bit with
/// `bitset` off the table, oldest first, calling `with` on each; answer how
/// many it counted.
///
/// A `count` of zero or less still takes one, as Linux's `futex_wake` does:
/// it counts a waiter before comparing against the limit. An entry `with`
/// does not count -- a stale entry of a vector waiter another word's wake
/// already took -- is taken off all the same and the walk goes on (the
/// module's documentation). What is taken is dropped after the lock, since a
/// key may hold the last reference to its object.
///
/// Keys in different buckets are taken bucket by bucket, in the order of
/// `keys`: oldest first holds within a bucket, which is all a single key
/// needs. Only the self-checks pass more than one.
fn rouse(keys: &[Key], count: i32, bitset: u32, with: With) -> usize {
    let limit = usize::try_from(count.max(1)).unwrap_or(1);
    let mut counted = 0_usize;
    let mut taken = Vec::new();
    for (at, key) in keys.iter().enumerate() {
        let bucket = key.bucket();
        // A bucket an earlier key shares has been looked through already.
        if keys
            .iter()
            .take(at)
            .any(|earlier| earlier.bucket() == bucket)
        {
            continue;
        }
        let mut table = bucket_lock(bucket).lock();
        let mut index = 0;
        while counted < limit
            && let Some(entry) = table.get(index)
        {
            if !keys.contains(&entry.key) || entry.bitset & bitset == 0 {
                index += 1;
                continue;
            }
            let entry = table.remove(index);
            if with(&entry) {
                counted += 1;
            }
            taken.push(entry);
        }
        drop(table);
    }
    drop(taken);
    counted
}

/// `FUTEX_REQUEUE` and, with `expected`, `FUTEX_CMP_REQUEUE`: rouse up to
/// `wake_count` waiters on `from` and move up to `move_count` more onto `to`.
/// Answers how many were roused or moved.
fn requeue(
    process: &dyn Host,
    (from, to): (u64, u64),
    shared: bool,
    wake_count: i32,
    move_count: i32,
    expected: Option<u32>,
) -> Result<usize, Errno> {
    let (Ok(wake_count), Ok(move_count)) =
        (usize::try_from(wake_count), usize::try_from(move_count))
    else {
        return Err(Errno::EINVAL);
    };
    let source = key(process.core(), from, shared)?;
    let target = key(process.core(), to, shared)?;
    let (from_bucket, to_bucket) = (source.bucket(), target.bucket());
    // Both buckets' locks, the lower-numbered first, so that two requeues
    // the opposite ways cannot each hold the lock the other waits for; one
    // lock when both keys hash to the same bucket.
    let lock_both = || {
        let first = bucket_lock(from_bucket.min(to_bucket)).lock();
        let second =
            (from_bucket != to_bucket).then(|| bucket_lock(from_bucket.max(to_bucket)).lock());
        (first, second)
    };
    // As `wait` reads its word: faulted in outside the lock, then read under
    // it without a fault, round again if the page went in between.
    let (mut first, mut second) = loop {
        let Some(expected) = expected else {
            break lock_both();
        };
        let _ = read_word(process.core(), from)?;
        let locked = lock_both();
        match read_present_word(process.core(), from)? {
            None => {}
            Some(word) if word != expected => return Err(Errno::EAGAIN),
            Some(_) => break locked,
        }
    };
    let (source_table, mut target_table) = match second.as_mut() {
        None => (&mut *first, None),
        Some(higher) if from_bucket < to_bucket => (&mut *first, Some(&mut **higher)),
        Some(higher) => (&mut **higher, Some(&mut *first)),
    };
    // Room on `to`'s bucket for every entry that may move, made before any
    // is taken off, so that memory refused changes nothing. Within one
    // bucket the entries taken off leave the room.
    if let Some(target_table) = target_table.as_deref_mut() {
        let movable = source_table
            .iter()
            .filter(|entry| entry.key == source)
            .count()
            .min(wake_count.saturating_add(move_count));
        if fallible::try_reserve(target_table, movable).is_err() {
            return Err(Errno::ENOMEM);
        }
    }
    let mut taken = 0_usize;
    let mut moved = Vec::new();
    // The keys that leave the table, dropped after the lock.
    let mut gone = Vec::new();
    let mut index = 0;
    while let Some(entry) = source_table.get(index) {
        if entry.key != source || taken >= wake_count.saturating_add(move_count) {
            index += 1;
            continue;
        }
        let mut entry = source_table.remove(index);
        // A stale entry of a vector waiter already woken through another
        // word is taken off and counted neither as roused nor as moved.
        if entry.sleeper.is_woken() {
            gone.push(entry.key);
            continue;
        }
        if taken < wake_count {
            if entry.sleeper.rouse(entry.slot) {
                taken += 1;
            }
            gone.push(entry.key);
        } else {
            taken += 1;
            gone.push(core::mem::replace(&mut entry.key, target.clone()));
            // Both locks are held: a waiter leaving reads this after taking
            // the old bucket's lock and follows it to the new one.
            if let Some(recorded) = entry.sleeper.bucket_of(entry.slot) {
                recorded.store(to_bucket, Ordering::Release);
            }
            moved.push(entry);
        }
    }
    // Onto the end of the queue, behind anything already waiting on `to`.
    target_table.unwrap_or(source_table).extend(moved);
    drop(second);
    drop(first);
    drop(gone);
    Ok(taken)
}

/// Rouse up to `count` waiters on `address` in `space`: what a process's end
/// does for the address `CLONE_CHILD_CLEARTID` or `set_tid_address` gave it.
/// Keyed as a shared wake, as Linux wakes it, so a waiter that left out
/// `FUTEX_PRIVATE_FLAG` on a word in shared memory is found too. Takes the
/// space's lock.
pub(crate) fn wake_address(space: &Arc<AddressSpace>, address: u64, count: i32) -> usize {
    rouse(
        &[Key::new(space, address, true)],
        count,
        FUTEX_BITSET_MATCH_ANY,
        rouse_entry,
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
    let keys = either_key(process.space(), address);
    let mut buckets: Vec<usize> = keys.iter().map(Key::bucket).collect();
    buckets.dedup();
    let mut count = 0;
    for bucket in buckets {
        let table = bucket_lock(bucket).lock();
        count += table
            .iter()
            .filter(|entry| keys.contains(&entry.key))
            .count();
        drop(table);
    }
    count
}

/// Take up to `count` waiters off `address`, keyed either way, and report
/// them woken without rousing them: a wake with the one bug a count cannot
/// see.
///
/// Exists for the self-check's negative control, which must show that the
/// check fails a wake like this one rather than trusting the count it returns.
pub(crate) fn forget_waiters(process: &Process, address: u64, count: i32) -> usize {
    rouse(
        &either_key(process.space(), address),
        count,
        FUTEX_BITSET_MATCH_ANY,
        |_| true,
    )
}

/// A private entry on a word of a vector waiter that a wake already took
/// through another of its words, and that has not yet taken this one off:
/// what a stale entry is between the wake and its waiter's own unlist.
///
/// Exists for the self-check of the rule that a wake does not count a stale
/// entry, which needs one listed at a moment of its choosing.
#[derive(Debug)]
pub(crate) struct StaleEntry(Arc<Sleeper>);

impl StaleEntry {
    /// Whether it was still on the table, taking it off if it was.
    pub(crate) fn remove(self) -> bool {
        unlist(&self.0, 1).is_some()
    }
}

/// List a [`StaleEntry`] on `address` in `process`'s space, keyed as a
/// private wait is: slot 1 of a sleeper woken through slot 0.
///
/// # Errors
///
/// `EINVAL` for a misaligned word, `ENOMEM` when memory is refused.
pub(crate) fn list_stale_entry(process: &Process, address: u64) -> Result<StaleEntry, Errno> {
    let key = key(process, address, false)?;
    let bucket = key.bucket();
    let mut buckets = fallible::try_with_capacity(2).map_err(|_| Errno::ENOMEM)?;
    buckets.extend([AtomicUsize::new(bucket), AtomicUsize::new(bucket)]);
    let sleeper = fallible::try_arc(Sleeper {
        task: None,
        woken: AtomicUsize::new(0),
        buckets,
    })
    .map_err(|_| Errno::ENOMEM)?;
    let entry = Entry {
        key,
        bitset: FUTEX_BITSET_MATCH_ANY,
        sleeper: Arc::clone(&sleeper),
        slot: 1,
    };
    let mut table = bucket_lock(bucket).lock();
    fallible::try_push(&mut table, entry).map_err(|_| Errno::ENOMEM)?;
    drop(table);
    Ok(StaleEntry(sleeper))
}

/// The bucket of [`TABLE`] a private wait on `address` in `process`'s space
/// is listed in: for the self-check, which needs two words in different
/// buckets.
pub(crate) fn bucket_of(process: &Process, address: u64) -> usize {
    Key::new(process.space(), address, false).bucket()
}

/// A private requeue of every waiter on `from` to `to` with the one bug a
/// table in buckets invites: each entry's key is changed where it lies, and
/// the entry is left in `from`'s bucket, where a wake on `to` never looks.
/// Answers how many it re-keyed.
///
/// Exists for the self-check's negative control, which must show that the
/// requeue check fails a requeue like this one.
pub(crate) fn requeue_without_moving(process: &Process, from: u64, to: u64) -> usize {
    let source = Key::new(process.space(), from, false);
    let target = Key::new(process.space(), to, false);
    let mut gone = Vec::new();
    let mut table = bucket_lock(source.bucket()).lock();
    for entry in table.iter_mut().filter(|entry| entry.key == source) {
        gone.push(core::mem::replace(&mut entry.key, target.clone()));
    }
    drop(table);
    let count = gone.len();
    drop(gone);
    count
}

/// Read a `struct timespec` of `width` as nanoseconds, by `ppoll`'s rules.
fn read_timespec(process: &dyn Host, at: u64, width: TimeWidth) -> Result<u64, Errno> {
    crate::syscall::poll::read_timespec(process.core(), at, width)
}
