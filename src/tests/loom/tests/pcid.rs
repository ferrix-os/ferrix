//! Leaving an address space's processor set with its translations kept
//! under a PCID (`docs/PCID.md` §3; the consultant's P2, ledger line 4734).
//!
//! Kernel sites: `AddressSpace::install` (join the set, then read the
//! shootdown count `begun`, then `pcid::install`, which compares the slot's
//! `synced` with it and flushes and records it, or keeps), and
//! `AddressSpace::begin_shootdown` (with the translation already out of the
//! tables: count the shootdown, then read the set) followed by
//! `smp::flush_tlb_pages`, which interrupts every processor the set named.
//!
//! The model: one page whose translation one processor may hold, as a
//! "stale" flag on its TLB under the space's slot. The processor installs
//! the space, uses the page (the TLB caches it), leaves (the slot keeps it:
//! the no-flush write), and installs it again; meanwhile the other thread
//! takes the translation down and runs the shootdown. The promise: *once the
//! shootdown has returned, the processor never uses the old translation*:
//! at its second install it either flushed the slot, or was interrupted
//! while it held the space, which drops the entry.
//!
//! The interrupt is modelled as the shootdown setting the processor's
//! pending flag, the processor answering it where its interrupts are open
//! (after each install, and once after leaving), and the shootdown counting
//! as returned once it is answered; an answer drops the entry only while
//! the space is current, since `invlpg` reaches the current PCID alone. Both
//! orders the argument rests on are `SeqCst`: the count before the set read,
//! the join before the count read.
//!
//! Controls, which `loom` must find failing: the count read before the join
//! (install's order reversed), the set read before the count (the
//! shootdown's order reversed), and the comparison left out (a hit always
//! keeps).

use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};
use loom::thread;

/// The preemption bound every model runs under, as the other models'.
const PREEMPTIONS: usize = 3;

fn model(body: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(body);
}

/// The space and the one processor.
struct Machine {
    /// The space's set: whether the processor is in it.
    joined: AtomicBool,
    /// The space's shootdown count, `Flushes::begun`.
    begun: AtomicU64,
    /// The page's translation, in the tables.
    mapped: AtomicBool,
    /// The processor's TLB holds the old translation under the space's slot.
    stale: AtomicBool,
    /// The slot's `synced`.
    synced: AtomicU64,
    /// A shootdown's interrupt, not yet answered.
    pending: AtomicBool,
    /// The shootdown has returned: everyone it asked has answered.
    returned: AtomicBool,
    /// The controls.
    count_before_join: bool,
    set_before_count: bool,
    no_compare: bool,
}

impl Machine {
    fn new(count_before_join: bool, set_before_count: bool, no_compare: bool) -> Machine {
        Machine {
            joined: AtomicBool::new(false),
            begun: AtomicU64::new(0),
            mapped: AtomicBool::new(true),
            stale: AtomicBool::new(false),
            synced: AtomicU64::new(0),
            pending: AtomicBool::new(false),
            returned: AtomicBool::new(false),
            count_before_join,
            set_before_count,
            no_compare,
        }
    }

    /// `AddressSpace::install` and `pcid::install`, the slot held already
    /// (`first` false) or new.
    fn install(&self, first: bool) {
        let begun = if self.count_before_join {
            let begun = self.begun.load(Ordering::SeqCst);
            self.join();
            begun
        } else {
            self.join();
            self.begun.load(Ordering::SeqCst)
        };
        let keep = !first && (self.no_compare || self.synced.load(Ordering::Relaxed) >= begun);
        if !keep {
            // The flushing write: the slot's translations gone.
            self.stale.store(false, Ordering::Relaxed);
            self.synced.store(begun, Ordering::Relaxed);
        }
        // Interrupts open again: a pending shootdown is answered here, with
        // the space current, by `invlpg`.
        self.answer(true);
    }

    /// `CpuMask::join`: a `SeqCst` read-modify-write. `loom` 0.7 treats
    /// `SeqCst` accesses as acquire and release only, and so would find the
    /// store-buffer outcome the kernel's single total order of `SeqCst`
    /// operations rules out (and x86's locked instruction rules out in
    /// hardware); the fence after it stands in for that order, as `loom`
    /// models `SeqCst` fences exactly.
    fn join(&self) {
        let _ = self.joined.fetch_or(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// `begin_shootdown`'s count, with the fence for the reason `join` gives.
    fn count(&self) {
        let _ = self.begun.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
    }

    /// The interrupt's handler on the processor: answer, dropping the entry
    /// only if the space is `current` -- `invlpg` reaches the current PCID
    /// alone.
    fn answer(&self, current: bool) {
        if self.pending.swap(false, Ordering::SeqCst) {
            if current {
                self.stale.store(false, Ordering::Relaxed);
            }
            // The one processor asked has answered: the shootdown returns.
            self.returned.store(true, Ordering::SeqCst);
        }
    }

    /// The processor uses the page: a hit on a cached entry, or a walk that
    /// caches it while the tables still have it.
    fn touch(&self) {
        if self.mapped.load(Ordering::SeqCst) {
            self.stale.store(true, Ordering::Relaxed);
        }
    }

    /// The no-flush write to another space, and leaving this one's set.
    /// The entry stays under the slot, used for nothing.
    fn leave(&self) {
        let _ = self.joined.fetch_and(false, Ordering::SeqCst);
    }

    /// Take the translation down and shoot: `begin_shootdown`, then
    /// `flush_tlb_pages` to the set it read.
    fn shoot(&self) {
        self.mapped.store(false, Ordering::SeqCst);
        let named = if self.set_before_count {
            let named = self.joined.load(Ordering::SeqCst);
            self.count();
            named
        } else {
            self.count();
            self.joined.load(Ordering::SeqCst)
        };
        if named {
            // The interrupt; `wait_for` returns once it is answered, which
            // `answer` marks.
            self.pending.store(true, Ordering::SeqCst);
        } else {
            self.returned.store(true, Ordering::SeqCst);
        }
    }
}

fn run(count_before_join: bool, set_before_count: bool, no_compare: bool) {
    model(move || {
        let machine = Arc::new(Machine::new(
            count_before_join,
            set_before_count,
            no_compare,
        ));
        let processor = {
            let machine = Arc::clone(&machine);
            thread::spawn(move || {
                machine.install(true);
                machine.touch();
                machine.leave();
                machine.answer(false);
                machine.install(false);
                // The use after the switch back: once the shootdown has
                // returned, it must not find the old translation.
                let returned = machine.returned.load(Ordering::SeqCst);
                if returned {
                    assert!(
                        !machine.stale.load(Ordering::Relaxed),
                        "the processor used a translation a returned shootdown took down"
                    );
                }
            })
        };
        machine.shoot();
        processor.join().unwrap();
    });
}

/// Verifies: L.user.169
#[test]
fn a_switch_back_never_uses_what_a_shootdown_took_down() {
    run(false, false, false);
}

#[test]
#[should_panic(expected = "a returned shootdown took down")]
fn control_the_count_read_before_the_join() {
    run(true, false, false);
}

#[test]
#[should_panic(expected = "a returned shootdown took down")]
fn control_the_set_read_before_the_count() {
    run(false, true, false);
}

#[test]
#[should_panic(expected = "a returned shootdown took down")]
fn control_a_hit_that_always_keeps() {
    run(false, false, true);
}
