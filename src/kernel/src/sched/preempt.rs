//! The preemption count: how many reasons the running context has not to be
//! switched out, and how many of them are locks.
//!
//! Raised by every [`crate::sync::SpinLock`] for as long as it is held, and
//! by hand ([`preempt_disable`]) by the idle task while it frees a stack. The
//! lock is the reason it exists: a ticket lock hands itself to whoever is
//! next in line whether or not that context is running, so a holder switched
//! out for the few instructions it holds the lock stalls every waiter for a
//! round of the run queue, and waiters switched out holding tickets pass the
//! stall on. The thousand-task check spent fifty seconds that way on a queue
//! lock that was plain; with the count, a holder is never switched out and
//! the lock is held for the instructions it covers and no longer.
//!
//! **A context with the count raised must not block** (FX-0503). The count
//! belongs to the processor, and a task that slept with it raised would leave
//! the processor unable to preempt whatever ran next, then lower the count on
//! whichever processor woke it. `schedule` stops the machine if it is asked
//! to switch with the count raised, and an enable that finds nothing to lower
//! stops it too, so the boot test finds any such holder.
//!
//! # One word in the processor's record, and no locked operation
//!
//! The count and the locks held are one `u64` in [`crate::smp::PerCpu`]: the
//! count in the low half, the locks in the high half. A lock adds
//! `(1 << 32) + 1`, a raise by hand adds 1. They were two arrays of atomics
//! indexed by the processor's number, raised and lowered with interrupts
//! masked around a locked add and a locked compare-exchange loop each: about
//! six locked operations and four interrupt saves and restores per lock
//! taken and let go (OPAQUE-KERNEL.md §9.5, §9.8 2b).
//!
//! **Why a plain update is enough** (Linux's `preempt_count` argument). The
//! word is written only by code running on its own processor: the task, or
//! an interrupt handler that interrupted it, and every handler leaves the
//! word as it found it, because each lock it takes it lets go before it
//! returns. A switch happens only with the count at zero. What can go wrong
//! is the update landing on another processor's word: the task finds its
//! processor's record, is preempted and moved, and changes the record of the
//! processor it left. That was the bug the masked window of the old code
//! closed. `arch::this_cpu_add` closes it without the old cost, and
//! `arch::percpu` argues it per architecture: on x86-64 one `GS`-relative
//! `xadd` without `lock`, with every entry that can reach it on the kernel's
//! `GS` base (condition 2 of the consultant's 2026-10-02 review); on AArch64
//! and ARMv7-A a load and a store with every exception that can take a lock
//! masked.
//!
//! **Reading another processor's word**, as a report does, is a whole-word
//! load on the two 64-bit architectures, which is single-copy atomic. On
//! ARMv7-A a `u64` load is two, so the word is kept there as two `u32`
//! halves, each read whole: a report may see the count and the locks from
//! instants a few instructions apart, never a half-written number.
//!
//! The sites are pointer-sized words stored `Relaxed` beside the count, read
//! by a report as whole words.

use core::panic::Location;
#[cfg(not(target_pointer_width = "64"))]
use core::sync::atomic::AtomicU32;
#[cfg(target_pointer_width = "64")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, AtomicPtr, Ordering};

use ferrix_sync::IrqControl;

use crate::arch;

/// One raise of the count, by hand.
const ONE: u64 = 1;

/// One lock: a raise of the count and one of the locks held.
const ONE_LOCK: u64 = (1 << 32) | ONE;

/// The count in a word: its low half.
const fn count_of(word: u64) -> u32 {
    word as u32
}

/// The locks held in a word: its high half.
const fn held_of(word: u64) -> u32 {
    (word >> 32) as u32
}

/// What one raise or lower changes.
const fn step(by_lock: bool) -> u64 {
    if by_lock { ONE_LOCK } else { ONE }
}

/// One processor's count and locks held, and where each was last raised.
/// A field of [`crate::smp::PerCpu`]; only this module reads or writes it.
#[derive(Debug)]
pub(crate) struct PreemptState {
    /// The count in the low half and the locks held in the high half.
    #[cfg(target_pointer_width = "64")]
    word: AtomicU64,
    /// The same as two halves, low first, so that each is read whole: see
    /// the module's last section.
    #[cfg(not(target_pointer_width = "64"))]
    word: [AtomicU32; 2],
    /// Where the count was last raised: the file and line that took the
    /// lock, kept so that a holder found asleep is named rather than counted.
    /// The lock it took last is the one it sleeps under. A pointer to a
    /// `'static` `Location`, or null.
    site: AtomicPtr<Location<'static>>,
    /// Where the *outermost* lock was taken: the site recorded when the
    /// locks held went from zero to one, and what a shootdown asked for under
    /// a lock names. [`Self::site`] is wrong for that, because every lock
    /// taken and released since has overwritten it: `channel_read`'s
    /// negative control named the VMO's page-list lock, released a moment
    /// before, instead of the topology lock it held.
    lock_site: AtomicPtr<Location<'static>>,
}

impl PreemptState {
    /// Where [`Self::word`] is in the state, for `smp::PREEMPT_WORD_OFFSET`.
    pub(crate) const WORD_OFFSET: usize = core::mem::offset_of!(Self, word);

    /// Nothing raised, nothing recorded.
    pub(crate) const fn new() -> Self {
        Self {
            #[cfg(target_pointer_width = "64")]
            word: AtomicU64::new(0),
            #[cfg(not(target_pointer_width = "64"))]
            word: [AtomicU32::new(0), AtomicU32::new(0)],
            site: AtomicPtr::new(core::ptr::null_mut()),
            lock_site: AtomicPtr::new(core::ptr::null_mut()),
        }
    }

    /// The count.
    fn count(&self) -> u32 {
        #[cfg(target_pointer_width = "64")]
        {
            count_of(self.word.load(Ordering::Relaxed))
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.word[0].load(Ordering::Relaxed)
        }
    }

    /// The locks held.
    fn held(&self) -> u32 {
        #[cfg(target_pointer_width = "64")]
        {
            held_of(self.word.load(Ordering::Relaxed))
        }
        #[cfg(not(target_pointer_width = "64"))]
        {
            self.word[1].load(Ordering::Relaxed)
        }
    }

    /// Record `site` as the last raise, and as the outermost lock's when
    /// `first_lock`.
    fn note(&self, site: &'static Location<'static>, first_lock: bool) {
        let site = core::ptr::from_ref(site).cast_mut();
        self.site.store(site, Ordering::Relaxed);
        if first_lock {
            self.lock_site.store(site, Ordering::Relaxed);
        }
    }
}

/// Whether the count is kept: from the moment the scheduler starts, as the
/// counts it replaced were made then. A lock taken before is not counted and
/// must not be released after (the enable would find nothing to lower),
/// which the old arrays required too.
static COUNTING: AtomicBool = AtomicBool::new(false);

/// Begin keeping the count. Once, as the scheduler starts.
///
/// **Only once every processor has its record** (the consultant's condition 1
/// on 2b): the count is changed through the per-CPU register with no check
/// that it names a record, so a processor counting before its install would
/// write at a zero or garbage base. The secondaries take locks before they
/// install theirs (the interrupt controller's, on Arm), which is safe only
/// because the count is not kept yet. So this stops the machine, FX-0506,
/// unless the boot processor's register leads to its own record and every
/// processor is online, which a secondary marks only after checking its own
/// register (`smp::secondary_main`).
pub(super) fn start_counting() {
    let topology = crate::smp::topology();
    let boot = crate::smp::record(0);
    let missing = topology.map_or(Some(0), |topology| {
        topology
            .cpus()
            .iter()
            .find(|cpu| !cpu.is_online())
            .map(|cpu| cpu.logical)
    });
    let boot_installed = boot.is_some_and(|boot| {
        crate::smp::this_cpu().is_some_and(|me| core::ptr::eq(me, boot))
            && arch::cpu_local_register() == core::ptr::from_ref(boot) as u64
    });
    if let Some(cpu) = missing.or(if boot_installed { None } else { Some(0) }) {
        crate::panic::fatal!(
            crate::panic::catalog::PREEMPT_COUNT_WITHOUT_RECORD,
            "the preemption count would start while processor {cpu} has not installed its \
             per-CPU record"
        );
    }
    COUNTING.store(true, Ordering::Release);
    // After the store, not before (the consultant's advisory on 875b66f3b):
    // from here a library lock that goes through the hooks is counted at
    // both ends, as a kernel lock is.
    install_library_hooks();
}

/// Whether the count is kept yet. `Relaxed`: the flag orders nothing, and
/// the words it guards have been zero since the records were made.
pub(super) fn counting() -> bool {
    COUNTING.load(Ordering::Relaxed)
}

/// Add `delta` to this processor's word and return what it held, with
/// `site` recorded once the count is raised; `None` before the count is
/// kept.
///
/// The add is `arch::this_cpu_add`, atomic against interrupts and migration.
/// The sites are stored after it: by then the count holds the task here, and
/// an interrupt between the two that takes a lock records its own site, which
/// this one then overwrites for a holder that is indeed the last to raise it.
fn raise(delta: u64, site: &'static Location<'static>) -> Option<u64> {
    if !counting() {
        return None;
    }
    // SAFETY: (SHARED) the count is kept only once the boot processor's
    // record is installed and every secondary has installed its own, and the
    // offset is the word's, which only its own processor writes.
    let old = unsafe { arch::this_cpu_add(crate::smp::PREEMPT_WORD_OFFSET, delta) };
    if let Some(me) = crate::smp::this_cpu() {
        me.preempt
            .note(site, delta == ONE_LOCK && held_of(old) == 0);
    }
    Some(old)
}

/// Take `delta` from this processor's word and return what it held, or
/// `Err` with what it held when the count and, for a lock, the locks held do
/// not cover it, having put the word back. `None` before the count is kept.
fn lower(delta: u64) -> Option<Result<u64, u64>> {
    if !counting() {
        return None;
    }
    // SAFETY: (SHARED) as in `raise`.
    let old = unsafe { arch::this_cpu_add(crate::smp::PREEMPT_WORD_OFFSET, delta.wrapping_neg()) };
    if covers(old, delta) {
        return Some(Ok(old));
    }
    // Put it back before anything else runs on it: the panic that follows
    // takes locks of its own. Meanwhile the word read huge, which holds the
    // task here and preempts nothing.
    // SAFETY: (SHARED) as in `raise`.
    let _ = unsafe { arch::this_cpu_add(crate::smp::PREEMPT_WORD_OFFSET, delta) };
    Some(Err(old))
}

/// [`covers`] for a release by a lock's guard or by hand, for the check that
/// an underflow is refused.
pub(super) const fn covers_for_check(word: u64, by_lock: bool) -> bool {
    covers(word, step(by_lock))
}

/// Whether `word` holds what lowering it by `delta` takes away.
const fn covers(word: u64, delta: u64) -> bool {
    count_of(word) >= count_of(delta) && held_of(word) >= held_of(delta)
}

/// The kernel's half of `ferrix_sync::PreemptControl`: this processor's
/// count.
pub(crate) struct Preempt;

// SAFETY: (SHARED) `disable` raises this processor's count and `preempt_on_irq_exit`
// switches nothing while it is raised; `enable` lowers it and makes the
// decision that was deferred. Both are no-ops before the scheduler keeps a
// count, when nothing can be switched out.
unsafe impl ferrix_sync::PreemptControl for Preempt {
    #[track_caller]
    fn disable() {
        let _ = raise(ONE_LOCK, Location::caller());
    }

    fn enable() {
        enable_from(true);
    }
}

/// [`Preempt`]'s two halves as functions, for the locks of the libraries
/// below the kernel, which cannot name it: `ferrix_sync::HookedPreempt`.
static HOOKS: ferrix_sync::PreemptHooks = ferrix_sync::PreemptHooks {
    disable: |site| {
        let _ = raise(ONE_LOCK, site);
    },
    enable: || enable_from(true),
};

/// Put this processor's count behind every `ferrix_sync::HookedPreempt`
/// lock: the VFS's dentries, mounts, open files and tmpfs renames, and
/// btrfs's inodes and caches. Until then they are plain ticket locks, whose
/// holders and waiters a timer interrupt switches out; on 2026-10-03 that
/// convoyed all four processors of a desktop, on a dentry and then on the
/// btrfs root's metadata, for minutes at a time.
///
/// Once, from [`start_counting`], just after the count starts to be kept, so
/// that every hooked `disable` from then on raises the count its `enable`
/// lowers. Installed before the store, a lock taken between the two would
/// have raised nothing and been lowered after. The other order has the
/// mirror of that gap -- a library guard taken before the install and let go
/// after it -- and what keeps both empty is when this runs: `kmain` starts
/// the scheduler before `fs::init` makes the first VFS or btrfs lock, so no
/// library guard exists yet. The kernel's own locks rest on the same
/// condition for the store itself: no guard is held across the count's
/// start.
fn install_library_hooks() {
    // SAFETY: (SHARED) `HOOKS` are `Preempt`'s own `disable` and `enable`,
    // which keep `PreemptControl`'s promises (see its `unsafe impl`), and a
    // `'static` published once. They are installed by `start_counting`,
    // once, just after the count starts to be kept, and before the first
    // library lock exists (`fs::init` runs after the scheduler starts), so
    // no library guard is alive across the install, as `install_preempt_hooks`
    // asks.
    let installed = unsafe { ferrix_sync::install_preempt_hooks(&HOOKS) };
    debug_assert!(installed, "the library hooks are installed once");
}

/// Keep the running context on this processor until the matching
/// [`preempt_enable`].
#[track_caller]
pub(crate) fn preempt_disable() {
    let _ = raise(ONE, Location::caller());
}

/// Undo one [`preempt_disable`], and if that was the last, make the decision
/// an interrupt asked for meanwhile.
///
/// Only with interrupts on: a lock dropped inside a masked section, such as
/// under an `IrqSpinLock`, leaves the decision to the interrupt exit that
/// masked section will end with. And only once the scheduler runs.
///
/// An enable that finds nothing to lower is not tolerated: it means the
/// count was raised on another processor than this one, and that processor
/// is now unpreemptible for good. It used to be floored at zero and
/// forgotten, which turned one lost increment into FX-0503 on an innocent
/// task some time later.
pub(crate) fn preempt_enable() {
    enable_from(false);
}

/// Undo one [`preempt_disable`] for a switch about to be made, with
/// interrupts masked: no deferred decision, because the switch is one
/// (`super::block_ending_hold`, F-69's last look). An enable that finds
/// nothing to lower stops the machine, as [`preempt_enable`]'s does.
pub(super) fn enable_for_switch() {
    if let Some(Err(old)) = lower(ONE) {
        unmatched(old, false);
    }
}

/// Count a lock held under the interrupt mask (`sync::try_lock_masked`,
/// the native round trip's fast path): one lock on this processor's word,
/// so that A3 (`require_preemption_on`, FX-0503) sees it as it sees any
/// guard's, but no site recorded and no deferred decision, which a masked
/// holder could not make anyway. Two plain per-CPU adds, no locked
/// operation.
pub(crate) fn raise_masked() {
    if counting() {
        // SAFETY: (SHARED) as in `raise`.
        let _ = unsafe { arch::this_cpu_add(crate::smp::PREEMPT_WORD_OFFSET, ONE_LOCK) };
    }
}

/// Take back [`raise_masked`]'s lock, as the masked guard drops; stops the
/// machine (FX-0503's catalogue entry) for one it finds nothing to lower.
pub(crate) fn lower_masked() {
    if let Some(Err(old)) = lower(ONE_LOCK) {
        unmatched(old, true);
    }
}

/// [`preempt_enable`], saying whether a lock's guard is what is being
/// released.
fn enable_from(by_lock: bool) {
    match lower(step(by_lock)) {
        None => {}
        Some(Ok(old)) => {
            if count_of(old) == 1 {
                decide_deferred();
            }
        }
        Some(Err(old)) => unmatched(old, by_lock),
    }
}

/// The count has come back to zero: make the decision an interrupt asked
/// for while it was raised, if interrupts are on.
///
/// The flag is read before it is swapped, so that the common release, with
/// nothing asked for, makes no locked operation. A request posted after the
/// read was posted by an interrupt on this processor, or by a kick whose
/// interrupt is on its way, and that interrupt's exit makes the decision.
fn decide_deferred() {
    if !super::started() {
        return;
    }
    let Some(cpu) = super::this_cpu() else {
        return;
    };
    if super::resched_asked(cpu) && arch::interrupts_enabled() && super::take_resched(cpu) {
        super::schedule();
    }
}

/// Stop the machine for an enable that found nothing to lower: FX-0503.
fn unmatched(old: u64, by_lock: bool) -> ! {
    let cpu = super::this_cpu().unwrap_or(usize::MAX);
    let site = preempt_site(cpu);
    let what = if by_lock && held_of(old) == 0 {
        "count of locks held"
    } else {
        "count"
    };
    crate::panic::fatal!(
        crate::panic::catalog::SCHEDULE_WITH_PREEMPTION_HELD,
        "a lock that disables preemption was released on processor {cpu}, whose {what} \
         was already zero: it was taken on another processor (this one's count was \
         last raised at {}:{})",
        site.map_or("?", |site| site.file()),
        site.map_or(0, Location::line),
    );
}

/// The count of the processor this runs on, read so that it is this
/// processor's (`arch::this_cpu_read`). For
/// [`super::may_block`], whose caller may be preempted and moved on either
/// side of the read but not inside it.
pub(super) fn count_here() -> u32 {
    if !counting() {
        return 0;
    }
    // SAFETY: (SHARED) as in `raise`; a read.
    count_of(unsafe { arch::this_cpu_read(crate::smp::PREEMPT_WORD_OFFSET) })
}

/// This processor's number, count and locks held, read together with
/// interrupts masked: for a check, which compares them across what it does.
pub(super) fn word_here() -> Option<(usize, u32, u32)> {
    let saved = <arch::Irq as IrqControl>::disable();
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    let answer =
        crate::smp::this_cpu().map(|me| (me.logical, me.preempt.count(), me.preempt.held()));
    core::sync::atomic::compiler_fence(Ordering::SeqCst);
    <arch::Irq as IrqControl>::restore(saved);
    answer
}

/// `cpu`'s state, if it has a record.
fn state_of(cpu: usize) -> Option<&'static PreemptState> {
    crate::smp::record(cpu).map(|record| &record.preempt)
}

/// How many reasons `cpu`'s running context has not to be switched out.
pub(crate) fn preempt_count(cpu: usize) -> u32 {
    state_of(cpu).map_or(0, PreemptState::count)
}

/// How many locks that disable preemption `cpu`'s running context holds,
/// not counting a [`preempt_disable`] made by hand: what a shootdown may not
/// be asked for under. The reaper raises the count by hand around freeing a
/// stack, and that free is a shootdown, by design.
pub(crate) fn locks_held(cpu: usize) -> u32 {
    state_of(cpu).map_or(0, PreemptState::held)
}

/// The file and line that last raised `cpu`'s count, if any is recorded.
pub(crate) fn preempt_site(cpu: usize) -> Option<&'static Location<'static>> {
    let pointer = state_of(cpu)?.site.load(Ordering::Relaxed);
    // SAFETY: (SHARED) only `PreemptState::note` stores here, and only a pointer
    // to a `'static` location the compiler handed it.
    unsafe { pointer.cast_const().as_ref() }
}

/// The file and line that took the outermost lock `cpu`'s running context
/// still holds, if any is recorded. Meaningful only while [`locks_held`] is
/// above zero.
pub(crate) fn lock_site(cpu: usize) -> Option<&'static Location<'static>> {
    let pointer = state_of(cpu)?.lock_site.load(Ordering::Relaxed);
    // SAFETY: (SHARED) only `PreemptState::note` stores here, and only a pointer
    // to a `'static` location the compiler handed it.
    unsafe { pointer.cast_const().as_ref() }
}
