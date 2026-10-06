//! A task's pending work: one word, read on every way back to user mode
//! (`docs/OPAQUE-KERNEL.md` §9.8, 2c).
//!
//! # Why a word
//!
//! On every return to ring 3 the core asked the personality whether the task
//! had anything to act on -- its process ending or stopping, a signal to
//! deliver, a mask to put back, a call to restart -- and the personality found
//! out by taking two references and two locks. Almost every return found
//! nothing. So each of those things has a bit here, and the way out
//! ([`look`]) reads the running task's word with interrupts masked: only when
//! a bit is set does it ask the personality, whose answer and actions are
//! unchanged.
//!
//! # The bits
//!
//! * [`END`]: the process is ending, or another thread's `execve` is replacing
//!   the program -- every case the personality's `must_leave` answers true.
//!   Termination's is the core's own: `object::process::Process::end_record`
//!   posts it on every task of the process, after it stores `terminated`.
//!   `execve`'s is the personality's. Never cleared: an ending process stays
//!   ending, and a thread told to leave leaves.
//! * [`STOP`]: the process is stopped.
//! * [`SIGNAL`]: a signal the thread may take, a saved mask to put back, or a
//!   call to restart.
//! * [`TRACE`]: reserved for a tracer's exit stop. Nothing posts it until
//!   `ptrace` exists, and the landing that brings `ptrace` brings its posters.
//! * [`FILTERED`]: reserved for §9.7's filtered flag. It waits on seccomp S3's
//!   install lock, which is not on `main`; nothing posts it, and the way out
//!   does not read it.
//!
//! # Posting and clearing
//!
//! A poster writes the state a bit stands for, then posts the bit: [`notify`]
//! posts, wakes the task and interrupts its processor, in that order, so "set
//! before the wake" holds by construction; [`post`] posts alone, for a task
//! that needs no wake; [`post_own`] posts to the running task. Only the task
//! clears its own bits, and only before it reads the state they stand for:
//! [`look`] clears [`STOP`] and [`SIGNAL`] as it finds them, and the way out
//! then asks the personality.
//!
//! **Why nothing posted is missed.** The poster writes the state (C) and then
//! posts (D, a `Release` read-modify-write); the task clears (A, an `Acquire`
//! read-modify-write) and then reads the state (B). A and D are
//! read-modify-writes of one word, so one comes first in its modification
//! order. D first: A reads D's value and synchronises with it, so C happens
//! before B and B sees the state. A first: D's bit survives A, so the task's
//! next look finds it -- the masked look that ends `return_to_user`, or the
//! next way out, which the interrupt `notify` sends a task in user mode
//! brings about. That holds in the language's model without a fence. Rust
//! emits, for the post and the clear: `lock or` and `lock and` on x86-64, a
//! plain `mov` for the look's load; on AArch64 `LDSETL` and `LDCLRA` with the
//! large-system extensions or exclusive-pair loops without them, and `LDAR`;
//! on ARMv7-A `LDREX`/`STREX` loops between `DMB ISH`s, and a load then
//! `DMB ISH`.
//!
//! # The check that the bits are complete
//!
//! A bit a poster forgot makes the way out skip work it owed. So with the
//! self-checks on (`ferrix.checks`, the default), a look that finds the word
//! clear asks the personality anyway ([`audit`]), and stops the machine
//! (FX-0520) if the answer is yes and no post is on its way. Every poster
//! brackets its write and its post in a [`Posting`], so that a look between
//! the two is not taken for a missing post.
//!
//! # The wake row
//!
//! A wake reads its target's state only under the run-queue lock of the
//! processor that owns the target, after re-reading which processor that is
//! under the lock, with no lock-free way out before it (`sched::wake_at_home`
//! and `wake_onto`). Step 4's fast path reads [`END`] under that lock with no
//! fence and relies on it. The check of it ([`check::run`]) reaches the item
//! through [`notify`]'s hook, which only stage 9 arms ([`arm`]) and the boot
//! requires disarmed after it, as it does F-60's leave hook (FX-0908).

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use ferrix_sync::IrqControl;

use super::Task;
use crate::arch;

pub(crate) mod check;

/// The process is ending, or another thread's `execve` is replacing the
/// program.
pub(crate) const END: u32 = 1 << 0;
/// The process is stopped.
pub(crate) const STOP: u32 = 1 << 1;
/// A signal to take, a saved mask to put back, or a call to restart.
pub(crate) const SIGNAL: u32 = 1 << 2;
/// Reserved: a tracer's exit stop. Nothing posts it until `ptrace` exists.
pub(crate) const TRACE: u32 = 1 << 3;
/// Reserved: the process is filtered (§9.7's T2 flag). Nothing posts it until
/// seccomp S3's install lock is on `main`, and the way out does not read it.
#[expect(
    dead_code,
    reason = "reserved for seccomp S3, whose install lock its post needs; held here so no other bit takes it"
)]
pub(crate) const FILTERED: u32 = 1 << 4;

/// The bits the way out acts on.
const ATTENTION: u32 = END | STOP | SIGNAL | TRACE;
/// The bits the task clears as it finds them. [`END`] is never cleared.
const CLEARED_AS_FOUND: u32 = STOP | SIGNAL;

/// Post `bits` to `task`, then wake it and interrupt its processor: the one
/// way a waker posts, so that the bits are set before the task can run.
pub(crate) fn notify(task: &Arc<Task>, bits: u32) {
    post(task, bits);
    wake_posted(task);
}

/// The second half of [`notify`], for a poster that posted with [`post`]
/// under a lock it must let go of before waking: wake `task` and interrupt
/// its processor.
pub(crate) fn wake_posted(task: &Arc<Task>) {
    // The bit before the wake's read of the task's state, paired with the
    // fence a trusting wait makes after it stores `BLOCKED`
    // (`WaitQueue::wait_sliced`), for a wait that reads the bit itself
    // (`syscall::native::receive_words`'s `END`): of the two fences one
    // comes first, and the side whose fence comes second sees the other's
    // store. The model is `src/tests/loom`.
    core::sync::atomic::fence(Ordering::SeqCst);
    if HOOK.load(Ordering::Acquire) != 0 {
        check::before_wake(task);
    }
    super::wake(task);
    super::interrupt(task);
}

/// Post `bits` to `task` without waking it: for a task that will look anyway
/// (one the poster wakes some other way, or a thread a process-directed
/// signal is posted to but not given to).
pub(crate) fn post(task: &Task, bits: u32) {
    let _ = task.work().fetch_or(bits, Ordering::Release);
}

/// Post `bits` to the running task, which needs no wake: it is on its way
/// back to user mode, where it looks.
pub(crate) fn post_own(bits: u32) {
    let _ = with_running(|task| post(task, bits));
}

/// Whether the running task has [`END`] posted: what a native wait asks in
/// place of the personality's `must_leave` (2e), which the core and the
/// personality post it for. False where nothing runs.
pub(crate) fn own_end() -> bool {
    with_running(has_end).unwrap_or(false)
}

/// Whether `task` has [`END`] posted: for a start that re-checks after it
/// lists its task, so as not to post twice.
pub(crate) fn has_end(task: &Task) -> bool {
    task.work().load(Ordering::Acquire) & END != 0
}

/// The way out's look, with interrupts masked: the running task's word, with
/// [`STOP`] and [`SIGNAL`] cleared if they were set -- the clear (A) before
/// the caller reads the state they stand for (B). Zero where nothing runs.
///
/// One load when the word is clear, which is every return that has nothing
/// to do.
pub(crate) fn look() -> u32 {
    with_running(|task| {
        let word = task.work().load(Ordering::Acquire);
        if word & CLEARED_AS_FOUND != 0 {
            task.work().fetch_and(!CLEARED_AS_FOUND, Ordering::Acquire)
        } else {
            word
        }
    })
    .unwrap_or(0)
}

/// The running task's word as it stands, clearing nothing: the frame tail's
/// look (`docs/OPAQUE-KERNEL.md` §9.7, part 2), which takes the general way
/// out, where [`look`] clears and reads, whenever any bit is set. Zero where
/// nothing runs. With interrupts masked.
#[cfg_attr(
    not(target_arch = "x86_64"),
    expect(dead_code, reason = "only x86-64's SYSCALL entry takes the fast path")
)]
pub(crate) fn peek() -> u32 {
    super::with_current(|task| task.work().load(Ordering::Acquire)).unwrap_or(0)
}

/// Whether a word [`look`] answered asks the way out to act.
pub(crate) const fn wants_attention(word: u32) -> bool {
    word & ATTENTION != 0
}

/// Run `visit` on the task running on this processor, with interrupts masked
/// so that it stays the running one. `None` before the scheduler has one.
///
/// The task is the processor record's borrow (2a, `sched::borrow`), not the
/// run queue's `current` under its lock, which this took for one load: so
/// [`look`], [`own_end`] and [`post_own`] take no lock.
fn with_running<R>(visit: impl FnOnce(&Task) -> R) -> Option<R> {
    let saved = <arch::Irq as IrqControl>::disable();
    let answer = super::with_current(visit);
    <arch::Irq as IrqControl>::restore(saved);
    answer
}

// ---------------------------------------------------------------------------
// The check that the bits are complete
// ---------------------------------------------------------------------------

/// How many posters are between the state they write and the bit they post,
/// counted only while the self-checks run: what lets [`audit`] tell a post
/// on its way from a post missed.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// A poster between its write (C) and its post (D). Made before the write,
/// dropped after the post; counted only while the self-checks run.
#[must_use = "a posting counts until it is dropped, after the post"]
pub(crate) struct Posting(bool);

/// Whether the self-checks run on this boot, as the composition root says
/// before any program runs ([`set_auditing`]): what turns [`audit`] on, and
/// what [`Posting`] counts under. The core's own copy, so that the core reads
/// no module above it.
static AUDITING: AtomicBool = AtomicBool::new(false);

/// Turn the check that the bits are complete on or off: `checks::init`,
/// once, as it reads `ferrix.checks` and before the first program.
pub(crate) fn set_auditing(on: bool) {
    AUDITING.store(on, Ordering::Release);
}

/// Whether [`audit`] runs at a clear word.
pub(crate) fn auditing() -> bool {
    AUDITING.load(Ordering::Acquire)
}

/// Begin a posting: see [`Posting`].
pub(crate) fn posting() -> Posting {
    let counted = auditing();
    if counted {
        let _ = IN_FLIGHT.fetch_add(1, Ordering::SeqCst);
    }
    Posting(counted)
}

impl Drop for Posting {
    fn drop(&mut self) {
        if self.0 {
            let _ = IN_FLIGHT.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// With the self-checks on, the way out's look when the word was clear: ask
/// `needs_attention` anyway, and stop the machine if it answers yes with no
/// post on its way. Answers whether the way out should act after all.
///
/// The order that makes a yes a missed post, and not a race: the poster
/// counts itself in [`IN_FLIGHT`] before its write (C) and out after its
/// post (D). A `needs_attention` that saw C was made after the count went
/// up; if the count reads zero afterwards, it went down after D, so the
/// second look sees D's bit. Only a yes with the count at zero and the word
/// still clear is a writer that posted nothing.
pub(crate) fn audit(needs_attention: fn() -> bool) -> bool {
    if !needs_attention() {
        return false;
    }
    if IN_FLIGHT.load(Ordering::SeqCst) != 0 {
        return true;
    }
    if wants_attention(look()) {
        return true;
    }
    crate::panic::fatal!(
        crate::panic::catalog::ATTENTION_WITHOUT_WORK,
        "the way back to user mode found its pending-work word clear and something to act on: \
         a writer of what the personality's needs_attention reads posted no bit (task {})",
        super::current_id().unwrap_or(0),
    );
}

// ---------------------------------------------------------------------------
// The hook only stage 9 arms
// ---------------------------------------------------------------------------

/// Which check has armed [`notify`]'s hook: zero for none, otherwise the
/// check's number in [`HOOKED_BY`] plus one. Set only by stage 9, cleared
/// before init, and required clear by the boot after stage 9 ([`hook_armed_by`],
/// FX-0908, as F-60's leave hook is): the rules
/// of `docs/OPAQUE-KERNEL.md` §9.7's condition 11. One static for every
/// check that hooks the wake, so step 4's case 14 arms this one too.
static HOOK: AtomicUsize = AtomicUsize::new(0);

/// The task the armed check watches, by id: zero for none.
static HOOK_TARGET: AtomicU64 = AtomicU64::new(0);

/// The checks that may arm the hook, by name, for the message that says one
/// was left armed.
const HOOKED_BY: &[&str] = &[
    "the wake row (sched::work::check)",
    "the fast path's last look (object::fast_path_check, case 14)",
];

/// The wake row's check, as [`HOOK`] records it.
pub(crate) const HOOK_WAKE_ROW: usize = 0;

/// Step 4's case 14, as [`HOOK`] records it: the fast path posts `END` to
/// its caller just before its last look (`docs/OPAQUE-KERNEL.md` §9.7, T13's
/// window, condition 11).
pub(crate) const HOOK_LAST_LOOK: usize = 1;

/// The fast path's hook, called just before T13's last look: with case 14's
/// check armed and `caller` its target, post `END` to it, as a kill landing
/// in that window would. Unarmed, one load.
pub(crate) fn fast_path_hook(caller: &Task) {
    if HOOK.load(Ordering::Acquire) != HOOK_LAST_LOOK + 1 {
        return;
    }
    if HOOK_TARGET.load(Ordering::Acquire) == caller.id {
        post(caller, END);
    }
}

/// Arm the hook for `check`, watching `target`. Stage 9's checks only.
///
/// The hook is one static for every check that hooks the wake, so a check
/// that arms it while another still holds it would overwrite that one, and
/// the next disarm would hide a hook left armed from the boot's check after
/// stage 9: that stops the machine here instead (FX-0908).
pub(crate) fn arm(check: usize, target: &Task) {
    if let Some(holder) = hook_armed_by() {
        crate::panic::fatal!(
            crate::panic::catalog::CHECK_HOOK_LEFT_ARMED,
            "a check's hook was still armed when another check armed it: {holder}, in \
             sched::work's hook"
        );
    }
    HOOK_TARGET.store(target.id, Ordering::Release);
    HOOK.store(check + 1, Ordering::Release);
}

/// Disarm the hook.
pub(crate) fn disarm() {
    HOOK.store(0, Ordering::Release);
    HOOK_TARGET.store(0, Ordering::Release);
}

/// The check that armed the hook, if one has.
fn armed_by() -> Option<usize> {
    HOOK.load(Ordering::Acquire).checked_sub(1)
}

/// The name of the check that left the hook armed, if one did: for the boot
/// check after stage 9 (`main.rs`'s `require_hooks_disarmed`, FX-0908),
/// which stops the machine before the marker and init if one did.
pub(crate) fn hook_armed_by() -> Option<&'static str> {
    armed_by().map(|check| HOOKED_BY.get(check).copied().unwrap_or("an unknown check"))
}

// ---------------------------------------------------------------------------
// A prepared task, for the process that lists it before it runs
// ---------------------------------------------------------------------------

impl super::PreparedTask {
    /// The task, which nothing has run yet: for its process to list
    /// (`object::process::Process::list_task`) before it is launched, so that
    /// an end recorded from then on reaches it. Here rather than beside the
    /// type, with the word it serves.
    pub(crate) const fn task(&self) -> &Arc<Task> {
        &self.task
    }
}
