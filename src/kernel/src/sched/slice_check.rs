//! The self-check of slice requests (L.sched.107, L.sched.108;
//! `docs/GAME-MODE-SCHED.md` §2.3): what a task's `sched_runtime` rests on.
//!
//! Three spinners pinned to the last processor, at the unit weight, so that
//! virtual and real time are the same nanoseconds: one asking for the
//! shortest request, one for none, and one for twice the largest slice. The
//! checker, on another processor, samples each one's deadline less its
//! virtual runtime under that queue's lock:
//!
//! * **Bounded** (both ids): no sample is ever more than the spinner's
//!   effective request, its own capped at the queue's slice, against the
//!   largest slice the queue can have, since a request granted while the
//!   queue held fewer tasks may be longer than its slice now. The short one
//!   past the floor is a renewal that ignored its request; the long one past
//!   the largest slice is a request the cap let through.
//! * **Measured with its own** (L.sched.107): a waiting spinner whose request
//!   was renewed and not yet run into sits exactly one effective request
//!   from its deadline, so each spinner is seen there at least once. A
//!   request ignored at every new request is never seen at the floor.
//!
//! Three attempts, each of a thousand samples: an exact sighting is a matter
//! of when the sample falls, a bound broken is not, and fails at once.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use ferrix_sched::{CpuSet, MIN_REQUEST_NS, NICE_0_WEIGHT};
use ferrix_sync::IrqControl;

use super::{SLICE_NS, Task};
use crate::arch;
use crate::smp::Topology;

/// What each spinner asks for: the floor, none, and past any slice.
const ASKS: [u64; 3] = [MIN_REQUEST_NS, 0, 2 * SLICE_NS];

/// Samples in one attempt, and the pause between two.
const SAMPLES: usize = 1_000;
const SAMPLE_GAP_NANOS: u64 = 200_000;

/// How long the spinners run before the first sample: long enough that each
/// has been given a request since its own was set.
const SETTLE_NANOS: u64 = 50_000_000;

/// Set to stop the spinners.
static STOP: AtomicBool = AtomicBool::new(false);
/// Spinners that have started, and that have stopped.
static STARTED: AtomicU64 = AtomicU64::new(0);
static STOPPED: AtomicU64 = AtomicU64::new(0);

/// A spinner: runs until told to stop.
fn spinner(_argument: usize) {
    let _ = STARTED.fetch_add(1, Ordering::AcqRel);
    while !STOP.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let _ = STOPPED.fetch_add(1, Ordering::AcqRel);
}

/// Run the check on the last processor, from this one.
///
/// # Errors
///
/// The sentence of the rule that was broken.
///
/// Verifies: `L.sched.107`, `L.sched.108`
pub(super) fn run(topology: &Topology) -> Result<(), &'static str> {
    let online = topology.online();
    if online < 2 {
        crate::console::println!("  sched    slice requests: one processor, not run");
        return Ok(());
    }
    let target = online - 1;
    STOP.store(false, Ordering::Release);
    STARTED.store(0, Ordering::Release);
    STOPPED.store(0, Ordering::Release);
    let mut tasks: Vec<Arc<Task>> = Vec::new();
    for (index, &ask) in ASKS.iter().enumerate() {
        let task = super::spawn_on(
            "slice-ask",
            spinner,
            index,
            NICE_0_WEIGHT,
            target,
            CpuSet::of(target),
        )?;
        super::set_request(&task, ask);
        tasks.push(task);
    }
    let answer = sample(&tasks, target);
    STOP.store(true, Ordering::Release);
    for task in &tasks {
        super::wait_until_gone(task, super::REAPER_PATIENCE_NANOS)?;
    }
    drop(tasks);
    answer?;
    crate::console::println!(
        "  sched    slice requests: each of three spinners measured with its own request, none past it"
    );
    Ok(())
}

/// The slice a spinner asking for `ask` must be given on a queue whose slice
/// is `slice`: L.sched.107 and 108 written out here rather than read from
/// `effective_request`, so that a control breaking that function is judged
/// by an answer it cannot also change.
fn expected(ask: u64, slice: u64) -> u64 {
    match ask {
        0 => slice,
        ask => ask.max(MIN_REQUEST_NS.min(slice)).min(slice),
    }
}

/// The attempts: a bound broken fails at once, and an attempt in which every
/// spinner was seen at its request passes.
fn sample(tasks: &[Arc<Task>], target: usize) -> Result<(), &'static str> {
    while STARTED.load(Ordering::Acquire) < ASKS.len() as u64 {
        super::sleep_for(1_000_000);
    }
    super::sleep_for(SETTLE_NANOS);
    if tasks
        .iter()
        .zip(ASKS)
        .any(|(task, ask)| task.request_ns() != ask)
    {
        return Err("a slice request did not reach its task");
    }
    for _attempt in 0..3 {
        let mut seen = [false; ASKS.len()];
        for _ in 0..SAMPLES {
            for ((task, ask), flag) in tasks.iter().zip(ASKS).zip(seen.iter_mut()) {
                *flag |= one_sample(task, ask, target)?;
            }
            super::sleep_for(SAMPLE_GAP_NANOS);
        }
        if seen.iter().all(|&flag| flag) {
            return Ok(());
        }
    }
    Err("a task's request was never measured with its own slice")
}

/// One look at `task`, asking for `ask`, on `target`'s queue: an error for a
/// bound broken, and whether it was seen waiting exactly one effective
/// request from its deadline.
fn one_sample(task: &Task, ask: u64, target: usize) -> Result<bool, &'static str> {
    let queue = super::queue_of(target).ok_or("no queue for the slice check's processor")?;
    // Interrupts off around the remote queue's lock, as every other taker of
    // it holds it: a wake on this processor may take the same lock.
    let saved = <arch::Irq as IrqControl>::disable();
    let sampled = queue.lock().request_sample(task.id);
    <arch::Irq as IrqControl>::restore(saved);
    let Some((offset, weight, asked, running, slice)) = sampled else {
        return Ok(false);
    };
    if weight != NICE_0_WEIGHT || asked != ask {
        return Err("a slice request did not reach its task's queue");
    }
    // Against the largest slice: a request granted while the queue held
    // fewer tasks may be longer than its slice now.
    if offset > expected(ask, SLICE_NS) as i64 {
        crate::console::println!(
            "  sched    slice requests: asked {ask} ns, {offset} ns from its deadline"
        );
        return Err(if ask == MIN_REQUEST_NS {
            "a renewed request ignored the task's slice"
        } else {
            "a request longer than the queue's slice was granted"
        });
    }
    Ok(!running && offset == expected(ask, slice) as i64)
}
