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

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::SpinLock;
use ferrix_linux_abi::errno::Errno;
use ferrix_linux_abi::types::{
    FUTEX_CLOCK_REALTIME, FUTEX_CMP_REQUEUE, FUTEX_PRIVATE_FLAG, FUTEX_REQUEUE, FUTEX_WAIT,
    FUTEX_WAIT_BITSET, FUTEX_WAKE, FUTEX_WAKE_BITSET,
};

use crate::object::process::{Host, Process};
use crate::sched::{self, Task, WaitQueue};
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

/// One task asleep in `FUTEX_WAIT`, as its wakers see it.
#[derive(Debug)]
struct Sleeper {
    /// The task to make runnable. `None` only before the scheduler starts,
    /// when a wait spins on `woken` instead.
    task: Option<Arc<Task>>,
    /// Set by the wake that took it off the table.
    woken: AtomicBool,
    /// The bucket of [`TABLE`] its entry is in, or was in last. Changed only
    /// by a requeue, holding both the old bucket's lock and the new one's.
    bucket: AtomicUsize,
}

impl Sleeper {
    /// Mark it woken and make its task runnable. Called with [`TABLE`] held.
    fn rouse(&self) {
        self.woken.store(true, Ordering::Release);
        if let Some(task) = &self.task {
            sched::wake(task);
        }
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
/// ended while it waits; `EINVAL` for a misaligned word, a zero bitset, a
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
                        crate::syscall::time::host_from(
                            &caller,
                            ferrix_linux_abi::types::CLOCK_MONOTONIC,
                            absolute,
                        )
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
        || sleeper.woken.load(Ordering::Acquire) || process.wait_interrupted(),
        deadline,
        TRUSTED_RECHECK_NANOS,
    );
}

/// Sleep on `address` if it holds `expected`, until a wake whose bitset
/// shares a bit with `bitset`, the caller is ended, or `deadline` passes.
/// `shared` is whether the call left out `FUTEX_PRIVATE_FLAG`.
///
/// **A wait that ends for none of those goes round again**, reading the word
/// afresh, as Linux's `futex_wait` does after a spurious wakeup. What ended it
/// can be gone by the time the answer is chosen: a `SIGSTOP` pending for the
/// process ends the wait, and another thread may take it before this one looks
/// again -- dequeued, and the process not yet marked stopped -- or the stop may
/// be over, continued while this thread had not yet run. Answered as a
/// timeout, a wait with none returned `ETIMEDOUT` from a stop: FX-0701's "a
/// `FUTEX_WAIT` stopped and continued returned instead of being restarted".
fn wait(
    process: &dyn Host,
    address: u64,
    shared: bool,
    expected: u32,
    bitset: u32,
    deadline: Option<u64>,
) -> Result<usize, Errno> {
    let key = key(process.core(), address, shared)?;
    let bucket = key.bucket();
    let sleeper = Arc::new(Sleeper {
        task: sched::current(),
        woken: AtomicBool::new(false),
        bucket: AtomicUsize::new(bucket),
    });
    let deadline = deadline.unwrap_or(u64::MAX);
    loop {
        loop {
            // Faulted in here, outside the lock, and read again under it
            // without a fault. Round again if the page went in between.
            let _ = read_word(process.core(), address)?;
            let mut table = bucket_lock(bucket).lock();
            match read_present_word(process.core(), address)? {
                None => {}
                Some(word) if word != expected => return Err(Errno::EAGAIN),
                Some(_) => {
                    sleeper.bucket.store(bucket, Ordering::Relaxed);
                    table.push(Entry {
                        key: key.clone(),
                        bitset,
                        sleeper: Arc::clone(&sleeper),
                    });
                    break;
                }
            }
        }

        sleep(&sleeper, process, deadline);

        // Off the table however it left, and under the lock, so that a waker
        // which found it has finished rousing it before this returns. The
        // entry goes after the lock, with the key it holds.
        drop(unlist(&sleeper));
        // Woken wins over the other two: a wake that took this waiter off the
        // table counted it, and reporting a timeout would lose that wake for
        // whoever the waker meant it for.
        if sleeper.woken.load(Ordering::Acquire) {
            return Ok(0);
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

/// Take `sleeper`'s entry off the table if it is still there, holding the
/// lock of the bucket it is in. A requeue may move it while this waits for
/// that lock, so the bucket is read again once the lock is held, and if it
/// changed, the lock of the one it names is taken instead.
fn unlist(sleeper: &Arc<Sleeper>) -> Option<Entry> {
    loop {
        let bucket = sleeper.bucket.load(Ordering::Acquire);
        let mut table = bucket_lock(bucket).lock();
        if sleeper.bucket.load(Ordering::Acquire) != bucket {
            continue;
        }
        let mine = table
            .iter()
            .position(|entry| Arc::ptr_eq(&entry.sleeper, sleeper))
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
    Ok(rouse(&[key], count, bitset, Sleeper::rouse))
}

/// Take up to `count` waiters on any of `keys` whose bitset shares a bit with
/// `bitset` off the table, oldest first, calling `with` on each.
///
/// A `count` of zero or less still takes one, as Linux's `futex_wake` does:
/// it counts a waiter before comparing against the limit. What is taken is
/// dropped after the lock, since a key may hold the last reference to its
/// object.
///
/// Keys in different buckets are taken bucket by bucket, in the order of
/// `keys`: oldest first holds within a bucket, which is all a single key
/// needs. Only the self-checks pass more than one.
fn rouse(keys: &[Key], count: i32, bitset: u32, with: fn(&Sleeper)) -> usize {
    let limit = usize::try_from(count.max(1)).unwrap_or(1);
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
        while taken.len() < limit
            && let Some(entry) = table.get(index)
        {
            if !keys.contains(&entry.key) || entry.bitset & bitset == 0 {
                index += 1;
                continue;
            }
            let entry = table.remove(index);
            with(&entry.sleeper);
            taken.push(entry);
        }
        drop(table);
    }
    taken.len()
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
    let (source_table, target_table) = match second.as_mut() {
        None => (&mut *first, None),
        Some(higher) if from_bucket < to_bucket => (&mut *first, Some(&mut **higher)),
        Some(higher) => (&mut **higher, Some(&mut *first)),
    };
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
        taken += 1;
        let mut entry = source_table.remove(index);
        if taken <= wake_count {
            entry.sleeper.rouse();
            gone.push(entry.key);
        } else {
            gone.push(core::mem::replace(&mut entry.key, target.clone()));
            // Both locks are held: a waiter leaving reads this after taking
            // the old bucket's lock and follows it to the new one.
            entry.sleeper.bucket.store(to_bucket, Ordering::Release);
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
        Sleeper::rouse,
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
        |_| {},
    )
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
