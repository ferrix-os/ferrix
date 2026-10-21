//! Waiting on a word of user memory, and being woken for it: the kernel's one
//! table of address waits (`docs/microkernel/K5.md`, `docs/MICROKERNEL.md` §3
//! row K5).
//!
//! A program's locks, condition variables and joins are a compare-and-swap
//! in user memory with this behind it for the slow path: sleep while the word
//! still holds the value the caller saw, until somebody who changed it wakes
//! the word. The native calls `address_wait`, `address_wake`,
//! `address_requeue` and `address_wait_any` (0x1068 to 0x106B) are answered
//! here, and so is the Linux personality's `futex`, which only decodes Linux's
//! arguments and calls in. There is one table, not two.
//!
//! # What a wait is keyed by
//!
//! A private key is the address space and the user address: two processes'
//! words at the same address are different words. The space is safe to name
//! by address: every waiter holds its process, and so the space, for as long
//! as it is on the table.
//!
//! A shared key is the memory object and the byte offset of the word in it:
//! the same word however many spaces map it, and at whatever address each
//! maps it. A call reaches a shared key two ways, which give the same key: by
//! an address in a shared region of its space (`SHARED`), or by a handle to
//! the object and an offset (`BY_VMO`), which needs no mapping at all. An
//! address no shared region holds gets a private key, which is Linux's answer
//! for a private mapping too. The key holds its object, so the object's
//! identity cannot be reused while anyone waits on it -- and so a key is
//! never dropped with a bucket's lock held, since the last reference to an
//! object may free its pages (`L.user.135`).
//!
//! # The lost wake-up
//!
//! A wait promises to sleep only if the word still holds the value the
//! caller saw, and a wake that follows a change must find every waiter that
//! saw the old value. Both are kept by one lock: the word is read, and the
//! waiter put on the table, while its key's bucket's lock is held, and a
//! waker takes the same lock to look (`L.user.136`).
//!
//! The read under the lock never faults and never commits memory
//! (`L.user.137`). Resolving a fault takes the space's lock and allocates a
//! frame, and filling an object's page may read a disk, none of which a spin
//! lock's holder may do. So the word is read first with nothing held,
//! faulting or filling its page in, and read again under the lock only if the
//! page is still there. If it went in between, the lock is let go and the
//! whole read done again.
//!
//! A waker rouses a waiter while still holding the lock, and a waiter leaving
//! for any reason takes the lock before it returns. So a wake can never land
//! after its waiter has gone on to sleep for something else.
//!
//! # Buckets, and what is never done under their locks
//!
//! The table is [`BUCKETS`] buckets, each with its own lock, and a key's
//! bucket is a hash of it keyed with a boot-random key ([`seed`]), so that a
//! program cannot aim its waits at one bucket. A requeue between two keys
//! takes both buckets' locks, the lower-numbered first, and a waiter records
//! which bucket each of its entries is in ([`Sleeper::buckets`]), changed
//! only with both locks held.
//!
//! **Nothing is allocated or freed under a bucket's lock.** A bucket is a
//! vector of entries; a listing pushes only into room it already has, and a
//! full bucket is swapped for a larger one allocated with nothing held
//! ([`grow`]). A bucket left holding under a quarter of its room is swapped
//! for a smaller one the same way ([`shrink`]). A wake takes entries off in
//! rounds of at most [`ROUND`] into an array on the stack and drops them after
//! the lock; a requeue reserves the room it needs before it takes the locks.
//!
//! **Charged.** A waiter's [`Sleeper`] carries a charge to the job of the
//! process that waits, for itself, its bucket records and one entry's room
//! per word, taken before anything is listed and given back when it leaves
//! (`L.user.144`). What a bucket holds beyond its live entries is at most
//! three times them, plus [`MIN_ROOM`] per bucket
//! (`docs/certification/MEMORY-AND-TIMING.md` §2.2r).
//!
//! # Many words at once
//!
//! A waiter may sleep on up to 128 words (its callers' bound) until any one of them
//! is woken, and is answered which (`L.user.140`). It is one [`Sleeper`] with
//! one entry per word -- its *slot* -- each listed in its own key's bucket.
//! A word that no longer holds its value unlists every slot listed so far,
//! unless one of them was woken in between, which is then the answer.
//!
//! Once a vector waiter is woken through one word, its entries on the others
//! stay listed until it runs and takes them off. A wake that meets such a
//! stale entry takes it off without counting it and goes on to the next, so
//! a wake never loses a waiter to a stale entry (`L.user.138`).
//!
//! # Lock order
//!
//! A bucket's lock, then, for a word read through its object, the object's
//! page lock, a leaf. A requeue takes the lower-numbered bucket first. A
//! space's lock is never taken under a bucket's lock: keys are made before
//! any is taken.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_kmem::Charge;

use crate::fallible;
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;

/// A bitset that matches every waiter.
pub(crate) const MATCH_ANY: u32 = 0xFFFF_FFFF;

/// Which word a waiter sleeps on. See the module documentation.
#[derive(Debug, Clone)]
pub(crate) enum Key {
    /// A word only one address space can name.
    Private {
        /// The address space, by address.
        space: usize,
        /// The word's user address.
        address: u64,
    },
    /// A word in an object that shared regions or handles name.
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

/// The boot's hash key, which [`seed`] sets once from the boot's random
/// bytes before any program runs. These values only stand until then.
static HASH_KEY: [AtomicU64; 2] = [
    AtomicU64::new(0x243F_6A88_85A3_08D3),
    AtomicU64::new(0x1319_8A2E_0370_7344),
];

/// Set the key [`Key::bucket`] hashes with: the item, once, from the boot's
/// random bytes, before any wait is listed (a wait listed before would be
/// looked for in another bucket by its wakers).
pub(crate) fn seed(key: [u64; 2]) {
    for (held, value) in HASH_KEY.iter().zip(key) {
        held.store(value, Ordering::Relaxed);
    }
}

/// The 128-bit product of `a` and `b`, folded to 64 bits: the mixing step of
/// [`Key::bucket`].
fn fold(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
    (product as u64) ^ ((product >> 64) as u64)
}

impl Key {
    /// The key for `address` in `space`: shared if `shared` asks for one and
    /// a shared region holds the word, private otherwise. Takes the space's
    /// lock, so never with a bucket's lock held.
    pub(crate) fn new(space: &Arc<AddressSpace>, address: u64, shared: bool) -> Key {
        if shared && let Some((object, offset)) = space.shared_object_at(address) {
            return Key::Shared { object, offset };
        }
        Key::Private {
            space: Arc::as_ptr(space).addr(),
            address,
        }
    }

    /// The key for the word `offset` bytes into `object`: the key a wait by
    /// address gets for the same byte through any shared mapping of it.
    pub(crate) fn in_object(object: Arc<Vmo>, offset: u64) -> Key {
        Key::Shared { object, offset }
    }

    /// The bucket of [`TABLE`] this key's waiters are listed in: the space or
    /// object and the word's place in it, mixed with the boot's key, whose
    /// top bits are the index.
    fn bucket(&self) -> usize {
        let (base, at) = match self {
            Key::Private { space, address } => (*space as u64, *address),
            Key::Shared { object, offset } => (Arc::as_ptr(object).addr() as u64, *offset),
        };
        let [first, second] = [
            HASH_KEY[0].load(Ordering::Relaxed),
            HASH_KEY[1].load(Ordering::Relaxed),
        ];
        let mixed = fold(base ^ first, at ^ second ^ 0x9E37_79B9_7F4A_7C15)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> (u64::BITS - BUCKET_BITS)) as usize
    }
}

/// Where a word is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    /// At this user address in the waiter's space.
    User(u64),
    /// Through its key's object, which must be [`Key::Shared`].
    Object,
}

/// One word a wait sleeps on: where it is read, the value it must still
/// hold, its key and the bitset its wakes must share a bit with.
#[derive(Debug, Clone)]
pub(crate) struct Word {
    /// Where it is read.
    pub(crate) place: Place,
    /// What it must hold for the wait to sleep.
    pub(crate) expected: u32,
    /// What it is keyed by.
    pub(crate) key: Key,
    /// Which wakes rouse it; never zero.
    pub(crate) bitset: u32,
}

/// Why a wait, wake or requeue answers without what was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WaitError {
    /// A word did not hold the value given.
    Changed,
    /// A word could not be read.
    Fault,
    /// Memory, or the job's limit on it, refused an allocation.
    NoMemory,
    /// The caller has to leave: its process is ending, or its personality
    /// has something for it.
    Interrupted,
    /// The deadline passed.
    TimedOut,
}

/// What [`Sleeper::woken`] holds until a wake takes the sleeper.
const NOT_WOKEN: usize = usize::MAX;

/// One task asleep on one or more words, as its wakers see it.
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
    /// What it costs its job: itself, `buckets`, and one entry's room per
    /// slot. Held for its drop, which gives the charge back.
    #[allow(dead_code, reason = "held for its drop, which gives the charge back")]
    charge: Charge,
}

impl Sleeper {
    /// Mark it woken through `slot` and make its task runnable, if no wake
    /// took it before: whether this one did. Called with the lock of the
    /// bucket the entry for `slot` was in held. A `false` is a stale entry,
    /// which the caller takes off without counting.
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
    /// Which wakes it answers to: a wake rouses it only if the two share a
    /// bit.
    bitset: u32,
    /// Who it is.
    sleeper: Arc<Sleeper>,
    /// Which of the sleeper's words this entry is.
    slot: usize,
}

/// What [`rouse`] does to each matching entry it takes off the table; it
/// answers whether the entry counts.
type With = fn(&Entry) -> bool;

/// [`With`] for a real wake.
fn rouse_entry(entry: &Entry) -> bool {
    entry.sleeper.rouse(entry.slot)
}

/// How long a wait sleeps between looks of its own: a second, since every
/// way a wait ends wakes it by name.
const TRUSTED_RECHECK_NANOS: u64 = 1_000_000_000;

/// Bits of a key's hash that pick its bucket.
const BUCKET_BITS: u32 = 8;

/// Buckets in [`TABLE`].
pub(crate) const BUCKETS: usize = 1 << BUCKET_BITS;

/// The least room a bucket keeps once it has had any: [`shrink`] goes no
/// lower.
pub(crate) const MIN_ROOM: usize = 4;

/// The most entries a wake takes off a bucket under one hold of its lock.
pub(crate) const ROUND: usize = 32;

/// Every waiter, by its key's bucket ([`Key::bucket`]), each bucket oldest
/// first, so that a wake rouses in arrival order.
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

/// Read the word with nothing held, faulting or filling its page in.
fn read_word(space: &AddressSpace, word: &Word) -> Result<u32, WaitError> {
    let mut bytes = [0_u8; 4];
    match (word.place, &word.key) {
        (Place::User(address), _) => {
            uaccess::copy_from_user(space, address, &mut bytes).map_err(|_| WaitError::Fault)?;
        }
        (Place::Object, Key::Shared { object, offset }) => {
            let (index, within) = (offset / PAGE_SIZE, (offset % PAGE_SIZE) as usize);
            let present = object
                .read_present(index, within, &mut bytes)
                .map_err(|_| WaitError::Fault)?;
            if !present && object.sourced(index) {
                object
                    .fill_one_page(index)
                    .map_err(|_| WaitError::Fault)?;
            }
        }
        (Place::Object, Key::Private { .. }) => return Err(WaitError::Fault),
    }
    Ok(u32::from_le_bytes(bytes))
}

/// Read the word with its bucket's lock held: `None` if its page is not there
/// to be read without a fault or a fill, which the caller answers by letting
/// go of the lock and reading it with [`read_word`].
///
/// The word is aligned and four bytes long, so it lies in one page and the
/// read is all or nothing. A page of an object that was never committed and
/// has nothing to fill it from reads as zero and stays uncommitted.
fn read_present_word(space: &AddressSpace, word: &Word) -> Result<Option<u32>, WaitError> {
    let mut bytes = [0_u8; 4];
    let read = match (word.place, &word.key) {
        (Place::User(address), _) => uaccess::copy_from_user_present(space, address, &mut bytes)
            .map_err(|_| WaitError::Fault)?,
        (Place::Object, Key::Shared { object, offset }) => {
            let (index, within) = (offset / PAGE_SIZE, (offset % PAGE_SIZE) as usize);
            object
                .read_present(index, within, &mut bytes)
                .map_err(|_| WaitError::Fault)?
                || !object.sourced(index)
        }
        (Place::Object, Key::Private { .. }) => return Err(WaitError::Fault),
    };
    Ok(read.then(|| u32::from_le_bytes(bytes)))
}

/// Make bucket `index` hold at least `room` entries: allocate a vector that
/// large with nothing held, then, under the lock, move the entries across and
/// swap it in if the bucket is still short of it. The vector given up is
/// freed after the lock.
///
/// # Errors
///
/// [`WaitError::NoMemory`] when the vector cannot be had.
fn grow(index: usize, room: usize) -> Result<(), WaitError> {
    let mut fresh: Vec<Entry> =
        fallible::try_with_capacity(room).map_err(|_| WaitError::NoMemory)?;
    let mut table = bucket_lock(index).lock();
    let old = if table.capacity() < room && fresh.capacity() >= table.len() {
        move_into(&mut table, &mut fresh);
        Some(core::mem::replace(&mut *table, fresh))
    } else {
        None
    };
    drop(table);
    drop(old);
    Ok(())
}

/// Move every entry of `table` into `fresh`, in order, without allocating:
/// the caller made sure `fresh` has the room.
fn move_into(table: &mut Vec<Entry>, fresh: &mut Vec<Entry>) {
    for entry in table.drain(..) {
        // Never refused: the caller checked the room. Were it, the entry
        // would be lost, not the machine.
        let _ = fallible::push_within(fresh, entry);
    }
}

/// Give back room bucket `index` holds beyond four times its entries, down to
/// [`MIN_ROOM`]: a smaller vector is allocated with nothing held and swapped
/// in under the lock if the bucket is still that empty; the larger is freed
/// after it. Memory refused leaves the bucket as it was.
fn shrink(index: usize) {
    let (len, room) = {
        let table = bucket_lock(index).lock();
        (table.len(), table.capacity())
    };
    if room <= MIN_ROOM || len.saturating_mul(4) >= room {
        return;
    }
    let Ok(mut fresh) = fallible::try_with_capacity::<Entry>((room / 2).max(MIN_ROOM)) else {
        return;
    };
    let mut table = bucket_lock(index).lock();
    let old = if table.len().saturating_mul(4) < table.capacity()
        && fresh.capacity() >= table.len()
        && fresh.capacity() < table.capacity()
    {
        move_into(&mut table, &mut fresh);
        Some(core::mem::replace(&mut *table, fresh))
    } else {
        None
    };
    drop(table);
    drop(old);
}

/// Block until `sleeper` is woken, `interrupted` says the caller must leave,
/// or `deadline` passes.
///
/// Trusting its wakes, as `poll` trusts a file's queues: a wake rouses the
/// sleeper's task by name, and a caller that must leave is woken by whatever
/// made it so.
fn sleep(sleeper: &Sleeper, interrupted: &dyn Fn() -> bool, deadline: u64) {
    let _ = WaitQueue::wait_on_any(
        &[&SLEEP],
        || sleeper.is_woken() || interrupted(),
        deadline,
        TRUSTED_RECHECK_NANOS,
    );
}

/// What a waiter on `words` words costs its job.
fn cost(words: usize) -> usize {
    ferrix_kmem::arc_footprint::<Sleeper>()
        .saturating_add(ferrix_kmem::buffer_footprint::<AtomicUsize>(words))
        .saturating_add(ferrix_kmem::buffer_footprint::<Entry>(words))
}

/// Sleep on every one of `words` that still holds its value, until a wake of
/// any of them, `interrupted` says the caller must leave, or `deadline`
/// passes; answer the index of the word whose wake took it. `space` is the
/// caller's, which a word at a user address is read in; `payer` is the quota
/// slot of the job the waiter is charged to.
///
/// **A wait that ends for none of those goes round again**, reading the words
/// afresh: what ended it can be gone by the time the answer is chosen (a stop
/// that is over, say), and a wait cut short for nothing must not answer a
/// timeout.
///
/// # Errors
///
/// [`WaitError::Changed`] if a word no longer holds its value when it is
/// listed and no word listed before it was woken in between;
/// [`WaitError::Fault`] for a word that cannot be read;
/// [`WaitError::NoMemory`] if the waiter cannot be made or charged, or a
/// bucket cannot grow; [`WaitError::Interrupted`]; [`WaitError::TimedOut`]
/// once `deadline` passes.
pub(crate) fn wait_on(
    space: &AddressSpace,
    words: &[Word],
    deadline: Option<u64>,
    interrupted: &dyn Fn() -> bool,
    payer: u32,
) -> Result<usize, WaitError> {
    let charge = Charge::to(payer, cost(words.len())).map_err(|_| WaitError::NoMemory)?;
    let mut buckets = fallible::try_with_capacity(words.len()).map_err(|_| WaitError::NoMemory)?;
    for word in words {
        // NOALLOC: the room for every word was reserved just above.
        let _ = fallible::push_within(&mut buckets, AtomicUsize::new(word.key.bucket()));
    }
    let sleeper = fallible::try_arc(Sleeper {
        task: sched::current(),
        woken: AtomicUsize::new(NOT_WOKEN),
        buckets,
        charge,
    })
    .map_err(|_| WaitError::NoMemory)?;
    let deadline = deadline.unwrap_or(u64::MAX);
    loop {
        for (slot, word) in words.iter().enumerate() {
            if let Err(error) = list(space, &sleeper, slot, word) {
                // Off the table, every slot listed before this one; one woken
                // in between is the answer.
                for listed in 0..slot {
                    leave(&sleeper, listed);
                }
                return match sleeper.woken.load(Ordering::Acquire) {
                    NOT_WOKEN => Err(error),
                    woken => Ok(woken),
                };
            }
        }

        sleep(&sleeper, interrupted, deadline);

        // Off the table however it left, and under the lock, so that a waker
        // which found it has finished rousing it before this returns.
        for slot in 0..words.len() {
            leave(&sleeper, slot);
        }
        // Woken wins over the other two: a wake that took this waiter off the
        // table counted it, and reporting a timeout would lose that wake for
        // whoever the waker meant it for.
        let woken = sleeper.woken.load(Ordering::Acquire);
        if woken != NOT_WOKEN {
            return Ok(woken);
        }
        if interrupted() {
            return Err(WaitError::Interrupted);
        }
        if crate::timer::now_nanos() >= deadline {
            return Err(WaitError::TimedOut);
        }
    }
}

/// List `sleeper`'s entry for `slot` on `word`, if the word still holds its
/// value: read first with nothing held, then again under its bucket's lock
/// without a fault, round again if the page went in between or the bucket
/// had to grow.
///
/// # Errors
///
/// [`WaitError::Changed`] if the word holds another value,
/// [`WaitError::Fault`] if it cannot be read, [`WaitError::NoMemory`] if the
/// bucket cannot grow.
fn list(
    space: &AddressSpace,
    sleeper: &Arc<Sleeper>,
    slot: usize,
    word: &Word,
) -> Result<(), WaitError> {
    let bucket = word.key.bucket();
    loop {
        let _ = read_word(space, word)?;
        let mut table = bucket_lock(bucket).lock();
        let Some(value) = read_present_word(space, word)? else {
            continue;
        };
        if value != word.expected {
            return Err(WaitError::Changed);
        }
        if table.len() == table.capacity() {
            let room = table.capacity().saturating_mul(2).max(MIN_ROOM);
            drop(table);
            grow(bucket, room)?;
            continue;
        }
        if let Some(recorded) = sleeper.bucket_of(slot) {
            recorded.store(bucket, Ordering::Relaxed);
        }
        let entry = Entry {
            key: word.key.clone(),
            bitset: word.bitset,
            sleeper: Arc::clone(sleeper),
            slot,
        };
        // NOALLOC: the room was checked under this same hold of the lock.
        let refused = fallible::push_within(&mut table, entry).err();
        drop(table);
        drop(refused);
        return Ok(());
    }
}

/// Take `sleeper`'s entry for `slot` off the table if it is still there,
/// holding the lock of the bucket it is in; then give back the bucket's room
/// if it is now mostly empty. A requeue may move the entry while this waits
/// for that lock, so the bucket is read again once the lock is held, and if
/// it changed, the lock of the one it names is taken instead. The entry is
/// dropped after the lock.
fn leave(sleeper: &Arc<Sleeper>, slot: usize) {
    let Some(recorded) = sleeper.bucket_of(slot) else {
        return;
    };
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
        drop(mine);
        shrink(bucket);
        return;
    }
}

/// Rouse up to `limit` waiters on `key` whose bitset shares a bit with
/// `bitset`, oldest first, and answer how many. A `limit` of zero rouses none.
pub(crate) fn wake(key: &Key, limit: usize, bitset: u32) -> usize {
    rouse(core::slice::from_ref(key), limit, bitset, rouse_entry)
}

/// Take up to `limit` waiters on any of `keys` whose bitset shares a bit
/// with `bitset` off the table, oldest first, calling `with` on each; answer
/// how many it counted.
///
/// An entry `with` does not count -- a stale entry of a vector waiter another
/// word's wake already took -- is taken off all the same and the walk goes
/// on. Entries are taken in rounds of at most [`ROUND`] per hold of the lock,
/// into an array on the stack, and dropped after it, since a key may hold the
/// last reference to its object.
///
/// Keys in different buckets are taken bucket by bucket, in the order of
/// `keys`. Only the self-checks pass more than one.
fn rouse(keys: &[Key], limit: usize, bitset: u32, with: With) -> usize {
    let mut counted = 0_usize;
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
        loop {
            let mut taken: [Option<Entry>; ROUND] = core::array::from_fn(|_| None);
            let mut full = false;
            let mut table = bucket_lock(bucket).lock();
            let mut index = 0;
            let mut held = 0;
            while counted < limit
                && let Some(entry) = table.get(index)
            {
                if !keys.contains(&entry.key) || entry.bitset & bitset == 0 {
                    index += 1;
                    continue;
                }
                let Some(place) = taken.get_mut(held) else {
                    full = true;
                    break;
                };
                let entry = table.remove(index);
                if with(&entry) {
                    counted += 1;
                }
                *place = Some(entry);
                held += 1;
            }
            drop(table);
            drop(taken);
            if !full {
                break;
            }
        }
        shrink(bucket);
    }
    counted
}

/// Rouse up to `wake_count` waiters on `source` and move up to `move_count`
/// more onto `target`, oldest first; with `compare`, only if the word
/// `compare` names still holds its expected value, read under both buckets'
/// locks. Answers how many were roused or moved.
///
/// Room for every entry that may move, and for the keys and entries taken
/// off, is made before anything is taken off: when it is short, both locks
/// go, the room is allocated, and the requeue starts again from the read.
///
/// # Errors
///
/// [`WaitError::Changed`] if the compared word holds another value,
/// [`WaitError::Fault`] if it cannot be read, [`WaitError::NoMemory`] if the
/// room cannot be had.
pub(crate) fn requeue(
    space: &AddressSpace,
    source: &Key,
    target: &Key,
    (wake_count, move_count): (usize, usize),
    compare: Option<&Word>,
) -> Result<usize, WaitError> {
    let (from_bucket, to_bucket) = (source.bucket(), target.bucket());
    let most = wake_count.saturating_add(move_count);
    // The keys a move replaces and the entries a wake takes off, dropped
    // after both locks.
    let mut replaced: Vec<Key> = Vec::new();
    let mut gone: Vec<Entry> = Vec::new();
    loop {
        if let Some(word) = compare {
            let _ = read_word(space, word)?;
        }
        // Both buckets' locks, the lower-numbered first, so that two
        // requeues the opposite ways cannot each hold the lock the other
        // waits for; one lock when both keys hash to the same bucket.
        let mut first = bucket_lock(from_bucket.min(to_bucket)).lock();
        let mut second =
            (from_bucket != to_bucket).then(|| bucket_lock(from_bucket.max(to_bucket)).lock());
        if let Some(word) = compare {
            match read_present_word(space, word)? {
                None => continue,
                Some(value) if value != word.expected => return Err(WaitError::Changed),
                Some(_) => {}
            }
        }
        let (source_table, target_table) = match second.as_mut() {
            None => (&mut *first, None),
            Some(higher) if from_bucket < to_bucket => (&mut *first, Some(&mut **higher)),
            Some(higher) => (&mut **higher, Some(&mut *first)),
        };
        let movable = source_table
            .iter()
            .filter(|entry| entry.key == *source)
            .count()
            .min(most);
        let target_room = target_table
            .as_ref()
            .map_or(0, |table| table.len().saturating_add(movable));
        let target_short = target_table
            .as_ref()
            .is_some_and(|table| table.capacity() < target_room);
        if target_short
            || replaced.capacity() < movable
            || gone.capacity() < movable
        {
            drop(second);
            drop(first);
            if target_short {
                grow(to_bucket, target_room)?;
            }
            fallible::try_reserve(&mut replaced, movable).map_err(|_| WaitError::NoMemory)?;
            fallible::try_reserve(&mut gone, movable).map_err(|_| WaitError::NoMemory)?;
            continue;
        }
        let taken = take_and_move(source_table, target_table, source, target, wake_count, most, to_bucket, (&mut replaced, &mut gone));
        drop(second);
        drop(first);
        drop(replaced);
        drop(gone);
        shrink(from_bucket);
        return Ok(taken);
    }
}

/// The body of [`requeue`] once both locks are held and the room is made:
/// rouse up to `wake_count` waiters on `source`, move the rest up to `most`
/// onto `target`'s bucket, and answer how many. Within one bucket (no
/// `target_table`) a moved entry goes to the end of the same one; only the
/// entries there when it began are looked at.
#[allow(clippy::too_many_arguments, reason = "the locked tables of one requeue")]
fn take_and_move(
    source_table: &mut Vec<Entry>,
    mut target_table: Option<&mut Vec<Entry>>,
    source: &Key,
    target: &Key,
    wake_count: usize,
    most: usize,
    to_bucket: usize,
    (replaced, gone): (&mut Vec<Key>, &mut Vec<Entry>),
) -> usize {
    let mut taken = 0_usize;
    let mut index = 0;
    let mut end = source_table.len();
    while index < end {
        let Some(entry) = source_table.get(index) else {
            break;
        };
        if entry.key != *source || taken >= most {
            index += 1;
            continue;
        }
        let mut entry = source_table.remove(index);
        end -= 1;
        // A stale entry of a vector waiter already woken through another
        // word is taken off and counted neither as roused nor as moved.
        if entry.sleeper.is_woken() {
            // NOALLOC: room for every movable entry was reserved.
            drop(fallible::push_within(gone, entry).err());
            continue;
        }
        if taken < wake_count {
            if entry.sleeper.rouse(entry.slot) {
                taken += 1;
            }
            // NOALLOC: as above.
            drop(fallible::push_within(gone, entry).err());
            continue;
        }
        taken += 1;
        let old = core::mem::replace(&mut entry.key, target.clone());
        // NOALLOC: as above.
        drop(fallible::push_within(replaced, old).err());
        // Both locks are held: a waiter leaving reads this after taking the
        // old bucket's lock and follows it to the new one.
        if let Some(recorded) = entry.sleeper.bucket_of(entry.slot) {
            recorded.store(to_bucket, Ordering::Release);
        }
        // NOALLOC: the target's room for every movable entry was made
        // before the locks were taken; within one bucket the entry's own
        // place was just freed. Onto the end of the queue, behind anything
        // already waiting on `target`.
        let table = match target_table.as_deref_mut() {
            Some(table) => table,
            None => &mut *source_table,
        };
        drop(fallible::push_within(table, entry).err());
    }
    taken
}

/// How many waiters are listed on `key` right now: for the self-checks,
/// which have to know a waiter is asleep before they can tell a wake that
/// works from one that arrived first.
pub(crate) fn waiters_on(keys: &[Key]) -> usize {
    let mut count = 0;
    for (at, key) in keys.iter().enumerate() {
        let bucket = key.bucket();
        if keys
            .iter()
            .take(at)
            .any(|earlier| earlier.bucket() == bucket)
        {
            continue;
        }
        let table = bucket_lock(bucket).lock();
        count += table
            .iter()
            .filter(|entry| keys.contains(&entry.key))
            .count();
        drop(table);
    }
    count
}

/// The bucket a key is listed in: for the self-checks, which need two words
/// in different buckets.
pub(crate) fn bucket_of(key: &Key) -> usize {
    key.bucket()
}

/// The entries and room of bucket `index`: for the self-check of the bound on
/// a bucket's uncharged room.
pub(crate) fn bucket_room(index: usize) -> (usize, usize) {
    let table = bucket_lock(index).lock();
    (table.len(), table.capacity())
}

/// The sabotage hooks the negative controls need: each breaks one of this
/// module's rules on purpose, so that a check can show it catches the break.
/// Compiled only into a kernel with its self-checks, and seen only by them
/// (K5's review, condition 4).
#[cfg(ferrix_checks = "on")]
pub(crate) mod sabotage {
    use super::{Arc, AtomicUsize, Charge, Entry, Key, MATCH_ANY, Sleeper};
    use super::{bucket_lock, fallible, leave, rouse};

    /// Take up to `limit` waiters off `keys` and report them woken without
    /// rousing them: a wake with the one bug a count cannot see.
    pub(crate) fn forget_waiters(keys: &[Key], limit: usize) -> usize {
        rouse(keys, limit, MATCH_ANY, |_| true)
    }

    /// A requeue of every waiter on `source` to `target` with the one bug a
    /// table in buckets invites: each entry's key is changed where it lies,
    /// and the entry is left in `source`'s bucket, where a wake on `target`
    /// never looks. Answers how many it re-keyed.
    pub(crate) fn requeue_without_moving(source: &Key, target: &Key) -> usize {
        let mut gone = alloc::vec::Vec::new();
        let mut table = bucket_lock(source.bucket()).lock();
        let count = table.iter().filter(|entry| entry.key == *source).count();
        if fallible::try_reserve(&mut gone, count).is_err() {
            return 0;
        }
        for entry in table.iter_mut().filter(|entry| entry.key == *source) {
            // NOALLOC: reserved above.
            drop(fallible::push_within(&mut gone, core::mem::replace(&mut entry.key, target.clone())).err());
        }
        drop(table);
        drop(gone);
        count
    }

    /// An entry on a word of a vector waiter that a wake already took
    /// through another of its words, and that has not yet taken this one off:
    /// what a stale entry is between the wake and its waiter's own leaving.
    #[derive(Debug)]
    pub(crate) struct StaleEntry(Arc<Sleeper>);

    impl StaleEntry {
        /// Whether it was still on the table, taking it off if it was.
        pub(crate) fn remove(self) -> bool {
            let listed = super::waiters_of(&self.0, 1);
            leave(&self.0, 1);
            listed
        }
    }

    /// List a [`StaleEntry`] on `key`: slot 1 of a sleeper woken through slot
    /// 0. Uncharged: only a self-check makes one.
    ///
    /// # Errors
    ///
    /// `Err(())` when memory is refused.
    pub(crate) fn list_stale_entry(key: Key) -> Result<StaleEntry, ()> {
        let bucket = key.bucket();
        let mut buckets = fallible::try_with_capacity(2).map_err(|_| ())?;
        // NOALLOC: reserved just above.
        let _ = fallible::push_within(&mut buckets, AtomicUsize::new(bucket));
        let _ = fallible::push_within(&mut buckets, AtomicUsize::new(bucket));
        let sleeper = fallible::try_arc(Sleeper {
            task: None,
            woken: AtomicUsize::new(0),
            buckets,
            charge: Charge::none(),
        })
        .map_err(|_| ())?;
        let entry = Entry {
            key,
            bitset: MATCH_ANY,
            sleeper: Arc::clone(&sleeper),
            slot: 1,
        };
        loop {
            let mut table = bucket_lock(bucket).lock();
            if table.len() == table.capacity() {
                let room = table.capacity().saturating_mul(2).max(super::MIN_ROOM);
                drop(table);
                super::grow(bucket, room).map_err(|_| ())?;
                continue;
            }
            // NOALLOC: the room was checked under this hold of the lock.
            let refused = fallible::push_within(&mut table, entry).err();
            drop(table);
            drop(refused);
            return Ok(StaleEntry(sleeper));
        }
    }
}

/// Whether `sleeper`'s entry for `slot` is on the table.
#[cfg(ferrix_checks = "on")]
fn waiters_of(sleeper: &Arc<Sleeper>, slot: usize) -> bool {
    let Some(recorded) = sleeper.bucket_of(slot) else {
        return false;
    };
    let table = bucket_lock(recorded.load(Ordering::Acquire)).lock();
    let listed = table
        .iter()
        .any(|entry| Arc::ptr_eq(&entry.sleeper, sleeper) && entry.slot == slot);
    drop(table);
    listed
}
