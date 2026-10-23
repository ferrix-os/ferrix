//! Kernel time, told from user time, task by task (`docs/THREAD-TIMES.md`
//! §2).
//!
//! A task's runtime is charged at every decision (`CpuQueue::account_with`)
//! as one sum. Its *kernel time* is the part from each entry from user mode
//! -- a system call, an interrupt, an exception -- to the next return to
//! user mode, and all of the time of a task with no user mode; `stime` in
//! `/proc` is that, `utime` the rest. It is counted the way Linux's
//! `VIRT_CPU_ACCOUNTING_GEN` counts it, with a counter reading at each
//! boundary, because Ferrix has no periodic tick for a sampled split to
//! read: a processor running one task gets none.
//!
//! # The state
//!
//! Two words a processor, on a line of their own: `since`, the reading at
//! which the running task last entered the kernel, or 0 while it is in user
//! mode (a reading of 0 is kept as 1, [`mark`]); and `pending`, the kernel
//! time of the stints it has finished since the last settle, in counter
//! ticks. **Only the processor itself reads or writes its words, with
//! interrupts masked** -- at its own entries and exits, and at its own
//! decisions ([`settle`], from `choose_next` and `current_runtime`), never
//! from `account_with`, which other processors run on this one's queue (a
//! wake's `insert`, `charge_running`). So the words need no lock, and relaxed
//! atomics are enough: nothing else ever looks at them. No pointer to a task
//! is kept: what is pending goes to the task the decision finds running, as
//! `queue.current` names it under its lock.
//!
//! [`settle`] moves `pending` and the open stint into the running task
//! ([`Task::add_kernel_ticks`], a load and a store under its queue's lock,
//! as `add_runtime` is; the lock's hand-over at a switch is what orders that
//! store before any later one from the processor the task moves to) and marks
//! the processor in the kernel from that reading on, which it is, whichever
//! task runs after the decision.
//!
//! # What takes no reading
//!
//! A stint is opened by an entry (an odd `since`, [`from_entry`]) or by a
//! decision (an even one, [`from_decision`]), and a return reads the counter
//! only to close one an entry opened: the span from a call's or a trap's
//! last decision -- the switch back to a task that blocked or was preempted
//! -- to its return counts as user time, so that a call that blocks pays one
//! reading, at its entry, and the decision's own (docs/THREAD-TIMES.md §5:
//! the round trip's general path, cut from four readings to none with the
//! next rule). On x86-64 the three native calls that block
//! (`channel_write_read`, `object_wait_one`, `port_wait`) take no entry
//! stamp at all: their time in the kernel up to their decision counts as
//! user time too. The native `channel_write_read` fast path enters before
//! the entry's stamp and leaves by the frame tail's quiet exit (or its
//! `Done` answer), so its own time in the kernel counts as user time; its
//! direct switch only settles what an earlier general call left
//! ([`direct_switch`], two loads and two branches when nothing is). The
//! entries that are never masked --
//! a non-maskable interrupt, a machine check and a debug exception from user
//! mode, which take the paranoid entry and never reach `trap::dispatch` --
//! take no reading either, and so never write the state under a stamp they
//! interrupted. And where the counter is the HPET (x86-64 with no invariant
//! TSC and no hypervisor to ask), nothing is counted at all ([`counting`]):
//! every stamp would be three device reads.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::task::Task;
use crate::arch;

/// Processors the state covers: as many as a domain can name, as
/// `IN_CALL`'s.
const PROCESSORS: usize = 256;

/// One processor's words, on a cache line of its own so that two
/// processors' stamps never share one.
#[repr(align(64))]
struct Slot {
    /// The counter reading at which the running task's stint in the kernel
    /// began, odd when an entry opened it and even when a decision did,
    /// never 0; 0 while it is in user mode.
    since: AtomicU64,
    /// Kernel time of stints that have ended since the last [`settle`], in
    /// counter ticks, not yet the running task's.
    pending: AtomicU64,
}

/// Every processor's [`Slot`].
static SLOTS: [Slot; PROCESSORS] = [const {
    Slot {
        since: AtomicU64::new(0),
        pending: AtomicU64::new(0),
    }
}; PROCESSORS];

/// Whether kernel time is counted on this boot: the counter is one register
/// read (`arch::cheap_counter_is_clock`). Decided once, by [`init`], before
/// the scheduler starts; every stamp and every settle reads it.
static COUNTING: AtomicBool = AtomicBool::new(false);

/// Decide whether kernel time is counted, and say so on the console. Once,
/// on the boot processor, with the clock chosen and before any task runs.
pub(crate) fn init() {
    let counting = arch::cheap_counter_is_clock();
    COUNTING.store(counting, Ordering::Relaxed);
    if counting {
        crate::console::println!(
            "  sched    kernel time counted at every entry and return, by the clock's own counter"
        );
    } else {
        crate::console::println!(
            "  sched    kernel time not counted: the clock's counter is a device read, so stime \
             stays 0"
        );
    }
}

/// Whether kernel time is counted on this boot.
#[inline]
pub fn counting() -> bool {
    COUNTING.load(Ordering::Relaxed)
}

/// This processor's slot, or `None` before its per-CPU record is installed
/// or where kernel time is not counted.
#[inline]
fn slot() -> Option<&'static Slot> {
    if !counting() {
        return None;
    }
    debug_assert!(
        !arch::interrupts_enabled(),
        "a kernel-time stamp with interrupts open"
    );
    crate::smp::this_cpu().and_then(|cpu| SLOTS.get(cpu.logical))
}

/// What [`slot`] asserts, for a caller that names its processor itself:
/// interrupts masked, and `cpu` this processor, so that only the owner ever
/// touches a slot. Debug builds alone, as `slot`'s.
#[inline]
fn debug_assert_this_cpu(cpu: usize) {
    debug_assert!(
        !arch::interrupts_enabled(),
        "a kernel-time settle with interrupts open"
    );
    debug_assert!(
        crate::smp::this_cpu().is_none_or(|here| here.logical == cpu),
        "a kernel-time settle on another processor's slot"
    );
}

/// A reading as `since` keeps it for a stint an entry opened: odd, so
/// never 0, which means "in user mode", and told from a decision's.
const fn from_entry(reading: u64) -> u64 {
    reading | 1
}

/// A reading as `since` keeps it for a stint a decision opened: even and
/// never 0. A return closes it with no reading of its own.
const fn from_decision(reading: u64) -> u64 {
    let even = reading & !1;
    if even == 0 { 2 } else { even }
}

/// The running task has entered the kernel from user mode: a system call,
/// an interrupt or an exception. With interrupts masked, on the entry's own
/// processor.
///
/// A stint still open here was never ended by a return to user mode, so
/// all of it is still the kernel's and goes to `pending`: time is counted
/// as kernel time until a return says otherwise, never dropped. On every
/// path that keeps the rules `since` is 0 here and this is a load and a
/// branch; the checks' kernel tasks that drive the entry themselves
/// (the x86-64 system call check's drivers) are the one case that is not.
#[inline]
pub(crate) fn entered() {
    let Some(slot) = slot() else {
        return;
    };
    let now = arch::cheap_counter();
    let since = slot.since.load(Ordering::Relaxed);
    if since != 0 {
        let pending = slot.pending.load(Ordering::Relaxed);
        slot.pending.store(
            pending.wrapping_add(now.wrapping_sub(since)),
            Ordering::Relaxed,
        );
    }
    slot.since.store(from_entry(now), Ordering::Relaxed);
}

/// [`entered`] for a trap that arrived `from_user`, and nothing for one
/// taken in the kernel, which is no stint's start: the trap dispatcher's
/// entry stamp. With interrupts masked.
#[inline]
pub(crate) fn trap_entered(from_user: bool) {
    if from_user {
        entered();
    }
}

/// The running task is about to return to user mode: its stint in the
/// kernel ends here. A load and a branch when no stint is open, as on the
/// fast path's quiet exit, and a store more when a decision opened it, whose
/// span to here counts as user time, with no reading. With interrupts
/// masked, after the last thing on the way out that could open them.
#[inline]
pub fn leaving() {
    let Some(slot) = slot() else {
        return;
    };
    let since = slot.since.load(Ordering::Relaxed);
    if since == 0 {
        return;
    }
    slot.since.store(0, Ordering::Relaxed);
    if since & 1 == 0 {
        return;
    }
    let stint = arch::cheap_counter().wrapping_sub(since);
    let pending = slot.pending.load(Ordering::Relaxed);
    slot.pending
        .store(pending.wrapping_add(stint), Ordering::Relaxed);
}

/// A decision on this processor, under its queue's lock with interrupts
/// masked: `running`, the task that ran up to it, is given the kernel time
/// it has made since the last settle, the stint still open included, and
/// the processor counts as in the kernel from `now` (the decision's own
/// counter reading, `timer::now_with_ticks`; read here when `None`),
/// whichever task the decision runs next.
pub(super) fn settle(running: Option<&Task>, now: Option<u64>) {
    if let Some(slot) = slot() {
        settle_slot(slot, running, now);
    }
}

/// [`settle`] on processor `cpu`, this one, which the caller already knows:
/// `choose_next`'s, with no look at the per-CPU record.
pub(super) fn settle_on(cpu: usize, running: Option<&Task>, now: Option<u64>) {
    if !counting() {
        return;
    }
    debug_assert_this_cpu(cpu);
    if let Some(slot) = SLOTS.get(cpu) {
        settle_slot(slot, running, now);
    }
}

/// [`settle`]'s work on `slot`.
#[inline]
fn settle_slot(slot: &Slot, running: Option<&Task>, now: Option<u64>) {
    let now = now.unwrap_or_else(arch::cheap_counter);
    let since = slot.since.load(Ordering::Relaxed);
    let mut ticks = slot.pending.load(Ordering::Relaxed);
    if since != 0 {
        ticks = ticks.wrapping_add(now.wrapping_sub(since));
    }
    slot.since.store(from_decision(now), Ordering::Relaxed);
    if ticks != 0 {
        slot.pending.store(0, Ordering::Relaxed);
        if let Some(task) = running {
            task.add_kernel_ticks(ticks);
        }
    }
}

/// The direct switch from `previous` on processor `cpu` (this one), which
/// entered by the fast path and so has no stint of an entry open: what an
/// earlier general call left pending goes to it, and a stint still open is
/// closed, into it with a reading should an entry have opened it, so that
/// the task switched to, which leaves by the frame tail's quiet exit, starts
/// in user time. Under the queue's lock with interrupts masked.
pub(super) fn direct_switch(cpu: usize, previous: &Task) {
    if !counting() {
        return;
    }
    debug_assert_this_cpu(cpu);
    let Some(slot) = SLOTS.get(cpu) else {
        return;
    };
    let since = slot.since.load(Ordering::Relaxed);
    let mut ticks = slot.pending.load(Ordering::Relaxed);
    if since != 0 {
        slot.since.store(0, Ordering::Relaxed);
        if since & 1 == 1 {
            ticks = ticks.wrapping_add(arch::cheap_counter().wrapping_sub(since));
        }
    }
    if ticks != 0 {
        slot.pending.store(0, Ordering::Relaxed);
        previous.add_kernel_ticks(ticks);
    }
}
