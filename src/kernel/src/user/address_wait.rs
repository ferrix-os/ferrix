//! Waiting on a word of user memory, and being woken for it: the kernel's one
//! table of address waits (`docs/microkernel/K5.md`, `docs/MICROKERNEL.md` §3
//! row K5).
//!
//! A program's locks, condition variables and joins are a compare-and-swap
//! in user memory with this behind it for the slow path: sleep while the word
//! still holds the value the caller saw, until somebody who changed it wakes
//! the word. The native calls `address_wait`, `address_wake`,
//! `address_requeue` and `address_wait_any` (0x106B to 0x106E) are answered
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
//! lock's holder may do. So the word is read under the lock without a fault
//! first, as Linux's `get_futex_value_locked`; only when its page is not
//! there is the lock let go, the word read with nothing held, faulting or
//! filling its page in, and the whole done again. The value compared is
//! always the one read under the lock.
//!
//! A waker rouses a waiter while still holding the lock, and a waiter leaving
//! for any reason takes the lock before it returns. So a wake can never land
//! after its waiter has gone on to sleep for something else.
//!
//! # The wake and the block (`L.user.246`)
//!
//! A waiter sleeps on no queue: nothing wakes a queue for it, since every way
//! a wait ends wakes its task by name. What keeps a wake from being lost
//! between the waiter's last look and its block is the order of four
//! accesses, two on each side: the waiter marks its task blocked, then looks
//! at its sleeper's `woken`; the waker marks `woken`, then reads the task's
//! state as it wakes it. That is the store-buffering shape, and a `SeqCst`
//! fence on each side between the two orders it: the waiter's in
//! `WaitQueue::wait_on_any`, after the task is marked blocked and before the
//! last look, and the waker's in [`Sleeper::rouse`], after the mark and
//! before the wake. Either the wake finds the task blocked and makes it
//! runnable, or the last look finds the mark and the wait does not block.
//!
//! Every waker of a sleeper, and what orders it against the block:
//! - a wake, and a requeue's wake: the fence pair above;
//! - a signal, a kill or an end, which the wait's `interrupted` looks for: the
//!   signal lock, which the poster holds as it posts and before it wakes, and
//!   the look takes after the task is marked blocked;
//! - the deadline, the caller's or [`TRUSTED_RECHECK_NANOS`]: the run queue's
//!   lock, under which the switch files it and the timer takes it out.
//!
//! The recheck stays as the defence it is: a wake lost to a fault in this
//! argument costs a second, not a hang.
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
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering, fence};

use ferrix_bootinfo::PAGE_SIZE;
use ferrix_kmem::Charge;

use crate::fallible;
use crate::sched::{self, Task, WaitQueue};
use crate::sync::SpinLock;
use crate::syscall::uaccess;
use crate::user::space::AddressSpace;
use crate::user::vmo::Vmo;

/// A bitset that matches every waiter.
pub const MATCH_ANY: u32 = 0xFFFF_FFFF;

/// Which word a waiter sleeps on. See the module documentation.
#[derive(Debug, Clone)]
pub enum Key {
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
pub fn seed(key: [u64; 2]) {
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
    pub fn new(space: &Arc<AddressSpace>, address: u64, shared: bool) -> Key {
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
    pub fn in_object(object: Arc<Vmo>, offset: u64) -> Key {
        Key::Shared { offset, object }
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
        // `checked_shr`: with no bits (ring 0 under `retyped`) every key is bucket 0.
        mixed.checked_shr(u64::BITS - BUCKET_BITS).unwrap_or(0) as usize
    }
}

/// Where a word is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    /// At this user address in the waiter's space.
    User(u64),
    /// Through its key's object, which must be [`Key::Shared`].
    Object,
}

/// One word a wait sleeps on: where it is read, the value it must still
/// hold, its key and the bitset its wakes must share a bit with.
#[derive(Debug, Clone)]
pub struct Word {
    /// Where it is read.
    pub place: Place,
    /// What it must hold for the wait to sleep.
    pub expected: u32,
    /// What it is keyed by.
    pub key: Key,
    /// Which wakes rouse it; never zero.
    pub bitset: u32,
}

/// Why a wait, wake or requeue answers without what was asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
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
        // The waker's half of the module's "The wake and the block": the mark
        // above is ordered before the state `wake` reads.
        fence(Ordering::SeqCst);
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
    Sleeper::rouse(&entry.sleeper, entry.slot)
}

/// How long a wait sleeps between looks of its own: a second, since every
/// way a wait ends wakes it by name.
const TRUSTED_RECHECK_NANOS: u64 = 1_000_000_000;

/// Bits of a key's hash that pick its bucket.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
const BUCKET_BITS: u32 = 8;

/// Ring 0's table under `kernel_memory=retyped` is one bucket over a fixed list (K16.9, ledger
/// 10576 Q): no bits pick a bucket. Ring 0 under `retyped` is always `ferrix_load_built = "none"`
/// (`retyped` requires `personality=user`, whose kernel holds no load); linuxd's copy of this
/// table, `"lift"`, keeps its 256 buckets in its own arena.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
const BUCKET_BITS: u32 = 0;

/// Buckets in [`TABLE`].
pub(crate) const BUCKETS: usize = 1 << BUCKET_BITS;

// CHECKS-ONLY: user/address_wait_check.rs, object/rehome_check.rs
/// What a program sees of the self-checks' compared sequence, in order: each call's count, then
/// the waiters left on the first word and on the second. One table for both memory models
/// (K16.9, ledger 10576 Q2): stage 9's program-driven case and stage 6's taskless one hold it.
#[doc(hidden)]
pub const SEQUENCE: [u8; 15] = [
    1, 2, 0, // a wake of one under bitset 0b10: the oldest, whose bitset is all
    1, 1, 1, // a compared requeue of one: the next oldest moves
    0, 1, 1, // a wake under 0b01 on the second word: its one waiter's bitset is 0b10
    1, 0, 1, // a wake of one on the first word: the last there
    1, 0, 0, // a wake of one on the second word: the moved waiter
];

// CHECKS-ONLY: user/address_wait_check.rs
/// [`BUCKETS`], for the self-check: one in ring 0 under `retyped` (K16.9).
#[doc(hidden)]
pub const BUCKET_COUNT: usize = BUCKETS;

// CHECKS-ONLY: user/address_wait_check.rs
/// The least room a bucket keeps once it has had any: [`shrink`] goes no
/// lower.
#[doc(hidden)]
pub const MIN_ROOM: usize = 4;

/// The most entries a wake takes off a bucket under one hold of its lock.
pub(crate) const ROUND: usize = 32;

/// Every waiter, by its key's bucket ([`Key::bucket`]), each bucket oldest
/// first, so that a wake rouses in arrival order.
static TABLE: [SpinLock<Bucket>; BUCKETS] = [const { SpinLock::new(Bucket::new()) }; BUCKETS];

/// A bucket: a vector that grows and shrinks, outside ring 0 under `retyped`.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
type Bucket = Vec<Entry>;

/// Add `entry` at the end of `table`, in its room: answered back if there is none.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn push_within(table: &mut Bucket, entry: Entry) -> Result<(), Entry> {
    fallible::push_within(table, entry)
}

/// Where an entry is in a bucket: its index under heap.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
type Cursor = usize;

/// The oldest entry's place in `table`.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn first(table: &Bucket) -> Option<Cursor> {
    (!table.is_empty()).then_some(0)
}

/// The entry at `at`.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn entry_at(table: &Bucket, at: Cursor) -> Option<&Entry> {
    table.get(at)
}

/// The place after `at`, in arrival order.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn after(table: &Bucket, at: Cursor) -> Option<Cursor> {
    let next = at.checked_add(1)?;
    (next < table.len()).then_some(next)
}

/// Take the entry at `at` off `table`, the ones after it moving up one, and answer it with
/// the place of the one that followed it.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn take_at(table: &mut Bucket, at: Cursor) -> (Option<Entry>, Option<Cursor>) {
    if at >= table.len() {
        return (None, None);
    }
    let taken = table.remove(at);
    (Some(taken), (at < table.len()).then_some(at))
}

/// Waiters ring 0's one bucket holds under `retyped`: `N_waiters`, as many as the tasks its pid
/// table holds, so the list cannot fill while waiters are tasks (ledger 10505 C4).
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
pub const FIXED_WAITERS: usize = crate::object::process::FIXED_TASKS;

/// No slot: the end of a list.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
const NIL: u16 = u16::MAX;

/// Ring 0's one bucket under `retyped`: a static of [`FIXED_WAITERS`] slots, the waiters in
/// arrival order by index links inside the one locked value, and the free slots on a list of
/// their own, so that nothing is made after boot, or at all (K16.9, L.object.339, H.MEM.33).
/// A removal relinks two neighbours and moves no entry; a walk follows the waiters present
/// (ledger 10576 Q1, 10693 S2).
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
#[derive(Debug)]
struct Bucket {
    /// The entries, each slot in use on the waiters' list, the rest on the free list.
    slots: [Option<Entry>; FIXED_WAITERS],
    /// A slot's next: the next younger waiter, or the next free slot.
    next: [u16; FIXED_WAITERS],
    /// A waiter's previous: the next older waiter.
    prev: [u16; FIXED_WAITERS],
    /// The oldest waiter, the youngest, the first slot given back, and the first never used.
    head: u16,
    tail: u16,
    free: u16,
    fresh: u16,
    /// How many are in use.
    len: usize,
}

/// Where an entry is in a bucket: its slot under `retyped`.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
type Cursor = u16;

/// A link to `slot`, or [`NIL`] for none.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
const fn link(slot: Option<u16>) -> u16 {
    match slot {
        Some(slot) => slot,
        None => NIL,
    }
}

/// The slot a link names, if any.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn slot_of(link: u16) -> Option<u16> {
    (link != NIL).then_some(link)
}

#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
impl Bucket {
    /// An empty bucket, every slot never used, made by the compiler.
    const fn new() -> Bucket {
        const _: () = assert!(FIXED_WAITERS < NIL as usize, "a slot's number fits a link");
        Bucket {
            slots: [const { None }; FIXED_WAITERS],
            next: [NIL; FIXED_WAITERS],
            prev: [NIL; FIXED_WAITERS],
            head: NIL,
            tail: NIL,
            free: NIL,
            fresh: 0,
            len: 0,
        }
    }

    /// A slot to fill: one given back, else the next never used; none when all are in use.
    fn take_free(&mut self) -> Option<u16> {
        if let Some(slot) = slot_of(self.free) {
            self.free = link(self.next_of(slot));
            return Some(slot);
        }
        let slot = self.fresh;
        (usize::from(slot) < FIXED_WAITERS).then(|| {
            self.fresh = slot + 1;
            slot
        })
    }

    fn len(&self) -> usize {
        self.len
    }

    fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// The link after `slot`.
    fn next_of(&self, slot: u16) -> Option<u16> {
        slot_of(*self.next.get(usize::from(slot))?)
    }

    /// The waiters, oldest first.
    fn iter(&self) -> impl Iterator<Item = &Entry> {
        core::iter::successors(slot_of(self.head), |&slot| self.next_of(slot))
            .take(self.len)
            .filter_map(|slot| self.slots.get(usize::from(slot))?.as_ref())
    }

    /// The waiters, in no order.
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut Entry> {
        self.slots.iter_mut().filter_map(Option::as_mut)
    }

    /// Set `slot`'s next, or the head when `slot` is none.
    fn set_next(&mut self, slot: Option<u16>, to: u16) {
        match slot {
            Some(slot) => {
                if let Some(cell) = self.next.get_mut(usize::from(slot)) {
                    *cell = to;
                }
            }
            None => self.head = to,
        }
    }

    /// Set `slot`'s previous, or the tail when `slot` is none.
    fn set_prev(&mut self, slot: Option<u16>, to: u16) {
        match slot {
            Some(slot) => {
                if let Some(cell) = self.prev.get_mut(usize::from(slot)) {
                    *cell = to;
                }
            }
            None => self.tail = to,
        }
    }
}

/// The oldest entry's place in `table`.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn first(table: &Bucket) -> Option<Cursor> {
    slot_of(table.head)
}

/// The entry at `at`.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn entry_at(table: &Bucket, at: Cursor) -> Option<&Entry> {
    table.slots.get(usize::from(at))?.as_ref()
}

/// The place after `at`, in arrival order.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn after(table: &Bucket, at: Cursor) -> Option<Cursor> {
    table.next_of(at)
}

/// Add `entry` at the end of `table`, in a free slot: answered back when none is free.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn push_within(table: &mut Bucket, entry: Entry) -> Result<(), Entry> {
    let Some(slot) = table.take_free() else {
        return Err(entry);
    };
    let Some(cell) = table.slots.get_mut(usize::from(slot)) else {
        return Err(entry);
    };
    *cell = Some(entry);
    let tail = slot_of(table.tail);
    table.set_next(Some(slot), NIL);
    table.set_prev(Some(slot), link(tail));
    table.set_next(tail, slot);
    table.tail = slot;
    table.len += 1;
    Ok(())
}

/// Take the entry at `at` off `table`, relinking its neighbours and freeing its slot, and
/// answer it with the place of the one that followed it. Moves no other entry.
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn take_at(table: &mut Bucket, at: Cursor) -> (Option<Entry>, Option<Cursor>) {
    let Some(taken) = table.slots.get_mut(usize::from(at)).and_then(Option::take) else {
        return (None, None);
    };
    let next = table.next_of(at);
    let prev = table.prev.get(usize::from(at)).copied().and_then(slot_of);
    table.set_next(prev, link(next));
    table.set_prev(next, link(prev));
    table.set_prev(Some(at), NIL);
    table.set_next(Some(at), table.free);
    table.free = at;
    table.len -= 1;
    (Some(taken), next)
}

/// Bucket `index` of [`TABLE`]. [`Key::bucket`] is the top [`BUCKET_BITS`]
/// of a hash, so every index is in range and the first bucket is never
/// answered in another's place; it is there because a lookup has to answer
/// something.
fn bucket_lock(index: usize) -> &'static SpinLock<Bucket> {
    TABLE.get(index).unwrap_or(&TABLE[0])
}

// CHECKS-ONLY: user/address_wait_check.rs
/// Reads of a waited word made with nothing held, faulting or filling its page
/// in, by [`list`], in the space [`FAULTING_SPACE`] names: one for each word
/// whose page was not there to be read under its bucket's lock.
#[doc(hidden)]
pub static FAULTING_READS: AtomicU64 = AtomicU64::new(0);
// CHECKS-ONLY: user/address_wait_check.rs
/// The address of the space whose faulting reads [`FAULTING_READS`] counts,
/// or 0 for none: the check's own, so that no other wait's are counted.
#[doc(hidden)]
pub static FAULTING_SPACE: AtomicUsize = AtomicUsize::new(0);

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
                object.fill_one_page(index).map_err(|_| WaitError::Fault)?;
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
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn grow(index: usize, room: usize) -> Result<(), WaitError> {
    let mut fresh: Vec<Entry> =
        fallible::try_with_capacity(room).map_err(|_| WaitError::NoMemory)?;
    let mut table = bucket_lock(index).lock();
    // The vector given up -- the old one, or the fresh one if the bucket
    // grew meanwhile -- freed after the lock either way.
    let unused = if table.capacity() < room && fresh.capacity() >= table.len() {
        move_into(&mut table, &mut fresh);
        core::mem::replace(&mut *table, fresh)
    } else {
        fresh
    };
    drop(table);
    drop(unused);
    Ok(())
}

/// Move every entry of `table` into `fresh`, in order, without allocating:
/// the caller made sure `fresh` has the room.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn move_into(table: &mut Vec<Entry>, fresh: &mut Vec<Entry>) {
    for entry in table.drain(..) {
        // Never refused: the caller checked the room. Were it, the entry
        // would be lost, not the machine.
        let _ = fallible::push_within(fresh, entry);
    }
}

/// Whether a bucket whose lock is held has room to give back by [`shrink`]'s
/// own test: more than [`MIN_ROOM`] and more than four times its entries.
/// Made under the hold that took entries out, so that a wake, a leave or a
/// requeue that leaves the bucket within that bound takes its lock no second
/// time; [`shrink`] makes the test again under its own.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn shrink_due(table: &Vec<Entry>) -> bool {
    table.capacity() > MIN_ROOM && table.len().saturating_mul(4) < table.capacity()
}

/// [`shrink`] bucket `index` if a held lock found it [`shrink_due`].
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn shrink_if(due: bool, index: usize) {
    if due {
        shrink(index);
    }
}

/// [`shrink_due`] for a requeue's source bucket, under its two locks: the
/// first, the lower bucket's, unless `source_higher` and there is a second.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
fn source_shrink_due(first: &Vec<Entry>, second: Option<&Vec<Entry>>, source_higher: bool) -> bool {
    match second {
        Some(higher) if source_higher => shrink_due(higher),
        _ => shrink_due(first),
    }
}

/// Give back room bucket `index` holds beyond four times its entries, down to
/// [`MIN_ROOM`]: a smaller vector is allocated with nothing held and swapped
/// in under the lock if the bucket is still that empty; the larger is freed
/// after it. Memory refused leaves the bucket as it was.
#[cfg(not(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift"))))]
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
    let unused = if table.len().saturating_mul(4) < table.capacity()
        && fresh.capacity() >= table.len()
        && fresh.capacity() < table.capacity()
    {
        move_into(&mut table, &mut fresh);
        core::mem::replace(&mut *table, fresh)
    } else {
        fresh
    };
    drop(table);
    drop(unused);
}

/// Ring 0's list under `retyped` neither grows nor shrinks: a full one refuses, fail closed,
/// which waiters at or under tasks never reach (ledger 10576 Q2).
#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn grow(_index: usize, _room: usize) -> Result<(), WaitError> {
    Err(WaitError::NoMemory)
}

#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn shrink_due(_table: &Bucket) -> bool {
    false
}

#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn shrink_if(_due: bool, _index: usize) {}

#[cfg(all(ferrix_kernel_memory = "retyped", not(ferrix_load_built = "lift")))]
fn source_shrink_due(_first: &Bucket, _second: Option<&Bucket>, _source_higher: bool) -> bool {
    false
}

/// Block until `sleeper` is woken, `interrupted` says the caller must leave,
/// or `deadline` passes.
///
/// Trusting its wakes, as `poll` trusts a file's queues: a wake rouses the
/// sleeper's task by name, and a caller that must leave is woken by whatever
/// made it so. On no queue, since nothing wakes one for it: the module's "The
/// wake and the block".
fn sleep(sleeper: &Sleeper, interrupted: &dyn Fn() -> bool, deadline: u64) {
    let _ = WaitQueue::wait_on_any(
        &[],
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

/// A sleeper for a wait on `words` by the running task, charged to `payer`'s
/// job before anything else is made.
///
/// # Errors
///
/// [`WaitError::NoMemory`] when the job's limit refuses the charge or memory
/// refuses the sleeper.
fn new_sleeper(words: &[Word], payer: u32) -> Result<Arc<Sleeper>, WaitError> {
    let charge = Charge::to(payer, cost(words.len())).map_err(|_| WaitError::NoMemory)?;
    let mut buckets = fallible::try_with_capacity(words.len()).map_err(|_| WaitError::NoMemory)?;
    for word in words {
        // NOALLOC: the room for every word was reserved just above.
        let _ = fallible::push_within(&mut buckets, AtomicUsize::new(word.key.bucket()));
    }
    fallible::try_arc(Sleeper {
        task: sched::current(),
        woken: AtomicUsize::new(NOT_WOKEN),
        buckets,
        charge,
    })
    .map_err(|_| WaitError::NoMemory)
}

/// List `sleeper` on every one of `words`, in order; if one cannot be listed,
/// take every slot listed before it off the table again and answer why.
///
/// # Errors
///
/// [`list`]'s.
fn list_every(
    space: &AddressSpace,
    sleeper: &Arc<Sleeper>,
    words: &[Word],
) -> Result<(), WaitError> {
    for (slot, word) in words.iter().enumerate() {
        if let Err(error) = list(space, sleeper, slot, word) {
            for listed in 0..slot {
                leave(sleeper, listed);
            }
            return Err(error);
        }
    }
    Ok(())
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
pub fn wait_on(
    space: &AddressSpace,
    words: &[Word],
    deadline: Option<u64>,
    interrupted: &dyn Fn() -> bool,
    payer: u32,
) -> Result<usize, WaitError> {
    let sleeper = new_sleeper(words, payer)?;
    let deadline = deadline.unwrap_or(u64::MAX);
    loop {
        if let Err(error) = list_every(space, &sleeper, words) {
            // One woken in between is the answer.
            return match sleeper.woken.load(Ordering::Acquire) {
                NOT_WOKEN => Err(error),
                woken => Ok(woken),
            };
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
/// value: read under its bucket's lock without a fault; when its page is not
/// there, read with nothing held, faulting it in, and round again, as when
/// the bucket had to grow.
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
    // A word whose page is there, as nearly every waited one is, is read once,
    // under the lock; one whose page is not is faulted in with nothing held and
    // read again under the lock (the module's "The lost wake-up").
    let mut fault_in = false;
    loop {
        if fault_in {
            if core::ptr::from_ref(space) as usize == FAULTING_SPACE.load(Ordering::Relaxed) {
                let _ = FAULTING_READS.fetch_add(1, Ordering::Relaxed);
            }
            let _ = read_word(space, word)?;
        }
        let mut table = bucket_lock(bucket).lock();
        let Some(value) = read_present_word(space, word)? else {
            fault_in = true;
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
        let refused = push_within(&mut table, entry).err();
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
        let mut at = first(&table);
        while let Some(here) = at
            && entry_at(&table, here).is_some_and(|entry| !(Arc::ptr_eq(&entry.sleeper, sleeper) && entry.slot == slot))
        {
            at = after(&table, here);
        }
        let mine = at.and_then(|here| take_at(&mut table, here).0);
        let due = shrink_due(&table);
        drop(table);
        drop(mine);
        shrink_if(due, bucket);
        return;
    }
}

/// Rouse up to `limit` waiters on `key` whose bitset shares a bit with
/// `bitset`, oldest first, and answer how many. A `limit` of zero rouses none.
pub fn wake(key: &Key, limit: usize, bitset: u32) -> usize {
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
        let due = loop {
            let mut taken: [Option<Entry>; ROUND] = core::array::from_fn(|_| None);
            let mut full = false;
            let mut table = bucket_lock(bucket).lock();
            let mut at = first(&table);
            let mut held = 0;
            while counted < limit
                && let Some(here) = at
                && let Some(entry) = entry_at(&table, here)
            {
                if !keys.contains(&entry.key) || entry.bitset & bitset == 0 {
                    at = after(&table, here);
                    continue;
                }
                let Some(place) = taken.get_mut(held) else {
                    full = true;
                    break;
                };
                let (entry, next) = take_at(&mut table, here);
                at = next;
                let Some(entry) = entry else {
                    break;
                };
                if with(&entry) {
                    counted += 1;
                }
                *place = Some(entry);
                held += 1;
            }
            let due = shrink_due(&table);
            drop(table);
            drop(taken);
            if !full {
                break due;
            }
        };
        shrink_if(due, bucket);
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
pub fn requeue(
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
        // The compared word faulted in with nothing held, read again below.
        let _ = compare.map_or(Ok(0), |word| read_word(space, word))?;
        // Both buckets' locks, the lower-numbered first, so that two
        // requeues the opposite ways cannot each hold the lock the other
        // waits for; one lock when both keys hash to the same bucket.
        let mut first = bucket_lock(from_bucket.min(to_bucket)).lock();
        let mut second =
            (from_bucket != to_bucket).then(|| bucket_lock(from_bucket.max(to_bucket)).lock());
        if !still_holds(space, compare)? {
            continue;
        }
        let (source_table, target_table) = match second.as_mut() {
            None => (&mut *first, None),
            Some(higher) if from_bucket < to_bucket => (&mut *first, Some(&mut **higher)),
            Some(higher) => (&mut **higher, Some(&mut *first)),
        };
        let (movable, target_short) =
            room_needed(source_table, target_table.as_deref(), source, most);
        if target_short.is_some() || replaced.capacity() < movable || gone.capacity() < movable {
            drop(second);
            drop(first);
            make_room(to_bucket, target_short, movable, (&mut replaced, &mut gone))?;
            continue;
        }
        let taken = take_and_move(
            source_table,
            target_table,
            source,
            target,
            wake_count,
            most,
            to_bucket,
            (&mut replaced, &mut gone),
        );
        let due = source_shrink_due(&first, second.as_deref(), from_bucket > to_bucket);
        drop(second);
        drop(first);
        drop(replaced);
        drop(gone);
        shrink_if(due, from_bucket);
        return Ok(taken);
    }
}

/// With a requeue's buckets' locks held: whether its compared word, if it has
/// one, still holds its value. `Ok(false)` when the word's page is not there
/// to be read without a fault, which the caller answers by letting go and
/// starting again.
///
/// # Errors
///
/// [`WaitError::Changed`] for another value, [`WaitError::Fault`].
fn still_holds(space: &AddressSpace, compare: Option<&Word>) -> Result<bool, WaitError> {
    let Some(word) = compare else {
        return Ok(true);
    };
    match read_present_word(space, word)? {
        None => Ok(false),
        Some(value) if value != word.expected => Err(WaitError::Changed),
        Some(_) => Ok(true),
    }
}

/// With a requeue's buckets' locks held: how many entries on `source` it may
/// take, at most `most`, and the room the target's bucket must have if it
/// has less (`None` when it has enough, or is the same bucket).
fn room_needed(
    source_table: &Bucket,
    target_table: Option<&Bucket>,
    source: &Key,
    most: usize,
) -> (usize, Option<usize>) {
    let movable = source_table
        .iter()
        .filter(|entry| entry.key == *source)
        .count()
        .min(most);
    let short = target_table.and_then(|table| {
        let room = table.len().saturating_add(movable);
        (table.capacity() < room).then_some(room)
    });
    (movable, short)
}

/// With no lock held: grow the target's bucket to `target_room` if it was
/// short, and make room for `movable` replaced keys and taken entries.
///
/// # Errors
///
/// [`WaitError::NoMemory`].
fn make_room(
    to_bucket: usize,
    target_room: Option<usize>,
    movable: usize,
    (replaced, gone): (&mut Vec<Key>, &mut Vec<Entry>),
) -> Result<(), WaitError> {
    if let Some(room) = target_room {
        grow(to_bucket, room)?;
    }
    fallible::try_reserve(replaced, movable).map_err(|_| WaitError::NoMemory)?;
    fallible::try_reserve(gone, movable).map_err(|_| WaitError::NoMemory)
}

/// The body of [`requeue`] once both locks are held and the room is made:
/// rouse up to `wake_count` waiters on `source`, move the rest up to `most`
/// onto `target`'s bucket, and answer how many. Within one bucket (no
/// `target_table`) a moved entry goes to the end of the same one; only the
/// entries there when it began are looked at.
#[allow(
    clippy::too_many_arguments,
    reason = "the locked tables of one requeue"
)]
fn take_and_move(
    source_table: &mut Bucket,
    mut target_table: Option<&mut Bucket>,
    source: &Key,
    target: &Key,
    wake_count: usize,
    most: usize,
    to_bucket: usize,
    (replaced, gone): (&mut Vec<Key>, &mut Vec<Entry>),
) -> usize {
    let mut taken = 0_usize;
    let mut at = first(source_table);
    // Only the entries there when it began: a move within one bucket goes to its end.
    let mut left = source_table.len();
    while left > 0
        && let Some(here) = at
        && let Some(entry) = entry_at(source_table, here)
    {
        left -= 1;
        if entry.key != *source || taken >= most {
            at = after(source_table, here);
            continue;
        }
        let (entry, next) = take_at(source_table, here);
        at = next;
        let Some(mut entry) = entry else {
            break;
        };
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
        drop(push_within(table, entry).err());
    }
    taken
}

/// How many waiters are listed on `key` right now: for the self-checks,
/// which have to know a waiter is asleep before they can tell a wake that
/// works from one that arrived first.
pub fn waiters_on(keys: &[Key]) -> usize {
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
pub fn bucket_of(key: &Key) -> usize {
    key.bucket()
}

// CHECKS-ONLY: user/address_wait_check.rs
/// The entries and room of bucket `index`: for the self-check of the bound on
/// a bucket's uncharged room, which only a kernel with its checks runs.
#[cfg(ferrix_checks = "on")]
#[doc(hidden)]
pub fn bucket_room(index: usize) -> (usize, usize) {
    let table = bucket_lock(index).lock();
    (table.len(), table.capacity())
}

/// The sabotage hooks the negative controls need: each breaks one of this
/// module's rules on purpose, so that a check can show it catches the break.
/// Compiled only into a kernel with its self-checks, and seen only by them
/// (K5's review, condition 4).
#[cfg(ferrix_checks = "on")]
pub mod sabotage {
    use super::{Arc, AtomicUsize, Charge, Entry, Key, MATCH_ANY, NOT_WOKEN, Sleeper};
    use super::{bucket_lock, fallible, leave, push_within, rouse};

    /// Take up to `limit` waiters off `keys` and report them woken without
    /// rousing them: a wake with the one bug a count cannot see.
    pub fn forget_waiters(keys: &[Key], limit: usize) -> usize {
        rouse(keys, limit, MATCH_ANY, |_| true)
    }

    /// A requeue of every waiter on `source` to `target` with the one bug a
    /// table in buckets invites: each entry's key is changed where it lies,
    /// and the entry is left in `source`'s bucket, where a wake on `target`
    /// never looks. Answers how many it re-keyed.
    pub fn requeue_without_moving(source: &Key, target: &Key) -> usize {
        let mut gone = alloc::vec::Vec::new();
        let mut table = bucket_lock(source.bucket()).lock();
        let count = table.iter().filter(|entry| entry.key == *source).count();
        if fallible::try_reserve(&mut gone, count).is_err() {
            return 0;
        }
        for entry in table.iter_mut().filter(|entry| entry.key == *source) {
            // NOALLOC: reserved above.
            drop(
                fallible::push_within(
                    &mut gone,
                    core::mem::replace(&mut entry.key, target.clone()),
                )
                .err(),
            );
        }
        drop(table);
        drop(gone);
        count
    }

    /// An entry on a word of a vector waiter that a wake already took
    /// through another of its words, and that has not yet taken this one off:
    /// what a stale entry is between the wake and its waiter's own leaving.
    #[derive(Debug)]
    pub struct StaleEntry(Arc<Sleeper>);

    impl StaleEntry {
        /// Whether it was still on the table, taking it off if it was.
        pub fn remove(self) -> bool {
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
    #[expect(clippy::result_unit_err, reason = "a check's hook: no error to name")]
    pub fn list_stale_entry(key: Key) -> Result<StaleEntry, ()> {
        list_taskless(key, MATCH_ANY, 0, 1).map(StaleEntry)
    }

    /// A waiter with no task, listed on one word: what a wake and a requeue find
    /// of a real one, without a program (K16.9, ledger 10576 Q2).
    #[derive(Debug)]
    pub struct Parked(Arc<Sleeper>);

    impl Parked {
        /// Whether a wake took it.
        pub fn woken(&self) -> bool {
            self.0.woken.load(core::sync::atomic::Ordering::Acquire) != NOT_WOKEN
        }

        /// Off the table, as its waiter's own leaving would take it.
        pub fn leave(self) {
            leave(&self.0, 0);
        }
    }

    /// List a [`Parked`] waiter on `key` under `bitset`. Uncharged: only a
    /// self-check makes one.
    ///
    /// # Errors
    ///
    /// `Err(())` when memory is refused.
    #[expect(clippy::result_unit_err, reason = "a check's hook: no error to name")]
    pub fn park(key: Key, bitset: u32) -> Result<Parked, ()> {
        list_taskless(key, bitset, NOT_WOKEN, 0).map(Parked)
    }

    /// A taskless sleeper whose entry for `slot` is listed on `key`.
    fn list_taskless(key: Key, bitset: u32, woken: usize, slot: usize) -> Result<Arc<Sleeper>, ()> {
        let bucket = key.bucket();
        let mut buckets = fallible::try_with_capacity(slot + 1).map_err(|_| ())?;
        for _ in 0..=slot {
            // NOALLOC: reserved just above.
            let _ = fallible::push_within(&mut buckets, AtomicUsize::new(bucket));
        }
        let sleeper = fallible::try_arc(Sleeper {
            task: None,
            woken: AtomicUsize::new(woken),
            buckets,
            charge: Charge::none(),
        })
        .map_err(|_| ())?;
        let entry = Entry {
            key,
            bitset,
            sleeper: Arc::clone(&sleeper),
            slot,
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
            let refused = push_within(&mut table, entry).err();
            drop(table);
            drop(refused);
            return Ok(sleeper);
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
