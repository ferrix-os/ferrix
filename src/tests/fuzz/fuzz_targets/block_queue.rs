//! Fuzz the block request queue with operation sequences a filesystem and a
//! driver could produce between them.
//!
//! From stage 11 every read a filesystem makes and every write it commits
//! passes through `src/lib/fs/common/block` in ring 0, and the order of submissions,
//! completions, failures and plugs is decided by everything running at once.
//! The queue's bugs are not crashes. They are a request answered twice or
//! never, a merged command whose range is not its parts', or a superblock
//! write that reached the disk ahead of the trees it names — which is found
//! only by a power cut, long after the code that caused it was merged.
//!
//! # The input
//!
//! Nine bytes of device and tuning — block size, a capacity of a few hundred
//! sectors, command limits, a depth of one to four, expiries of a few ticks,
//! a plug threshold and a request limit — then operations until the bytes run
//! out: submit (aimed, most of the time, to abut the last request), dispatch,
//! complete, requeue, hand back a stale or invented token, plug, unplug, and
//! let time pass. The harness then unplugs and drains the queue.
//!
//! # The properties
//!
//! Not panicking is the floor. Beyond it, `ferrix_block::model` checks every
//! step against a flat model of the rules — the same harness the crate's
//! seeded tests drive — and this target fails on the first rule broken:
//!
//! 1. **Validation** agrees with the model, error for error.
//! 2. **Every request completes exactly once**, with its own command's result.
//! 3. **A unit's range is exactly its parts' ranges**, contiguous, in limits.
//! 4. **Barrier order**: nothing submitted before a flush or FUA write is
//!    incomplete when it leaves; nothing submitted after it leaves before it
//!    completes; no merge crosses one.
//! 5. **Expiry**: an overdue request is never passed over by a less overdue
//!    unit in the same epoch.
//! 6. **Liveness**: dispatch returns nothing only when plugging, the depth or
//!    barrier order says it must, and the final drain empties the queue.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Err(violation) = ferrix_block::model::run(data) {
        panic!("{violation:?}");
    }
});
