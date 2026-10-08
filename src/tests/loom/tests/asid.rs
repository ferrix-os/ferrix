//! ARMv7-A's ASID allocator (`arch::armv7a::asid`, `L.armv7a.19`;
//! `docs/OPAQUE-KERNEL.md` §9.13, the second reading's B2): the install's
//! lock-free fast path against another processor's rollover.
//!
//! Kernel sites: `asid::number_for` (the generation load, the space's tag
//! load, and the swap of this processor's active tag), `asid::slow_path`
//! (under the lock: a number given, a rollover that swaps every processor's
//! active tag with 0 and reserves what it held, the flush a rollover left
//! pending, the active tag stored), and `ferrix_paging::asid::Numbers`,
//! restated here small: four numbers, so that a rollover comes at once.
//!
//! The oracle keeps, per processor, the space it runs and the TLB it has:
//! every number it ran since its last flush and the space it ran it for.
//! A processor is taken to run no program from the start of its install to
//! its end, where the kernel's `TTBR0` write is; what its old number can do
//! meanwhile is a speculative walk, which leaves entries in its TLB, and the
//! TLB below holds them. After every install it asserts three promises:
//! - *One meaning*: no two processors run one number for two spaces
//!   (DDI 0406C.d B3.9.1).
//! - *No stale entry*: a processor never runs a number its TLB holds for
//!   another space.
//! - *Reserved until flushed*: a processor that has not flushed since a
//!   rollover runs only its reserved number.
//!
//! The start: generation 2, numbers 1 to 3 all given (S0, S1, and a dead
//! space D), processor 0 running S0 and processor 1 running S1, both with
//! D's entry under number 3 still in their TLB, and a space O whose tag is
//! of generation 1. Processor 0 installs S0 and then S2; processor 1
//! installs S2 (which rolls over) and then O.
//!
//! Each control must make `loom` find a failing interleaving: the fast
//! path's swap made a load and a store; the rollover reading the active
//! tags without clearing them; reservations matched by number instead of
//! by tag.

use loom::sync::atomic::{AtomicU64, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

/// The preemption bound every model runs under, as the other models'.
const PREEMPTIONS: usize = 3;

fn model(body: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(PREEMPTIONS);
    builder.check(body);
}

/// Numbers in the model: 0 (never given) to 3.
const NUMBERS: usize = 4;
const CPUS: usize = 2;

/// The spaces.
const S0: usize = 0;
const S1: usize = 1;
const S2: usize = 2;
const DEAD: usize = 3;
const OLD: usize = 4;
const SPACES: usize = 5;

const fn tag(generation: u64, number: u64) -> u64 {
    (generation << 8) | number
}

const fn number_of(tag: u64) -> u64 {
    tag & 0xFF
}

/// Which sabotage, if any.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Control {
    None,
    SplitSwap,
    RolloverLeavesActive,
    ReservedByNumber,
}

/// `ferrix_paging::asid::Numbers`, small.
struct Numbers {
    generation: u64,
    taken: [bool; NUMBERS],
    next: usize,
    reserved: [u64; CPUS],
    pending: [bool; CPUS],
}

/// The model's machine.
struct Machine {
    control: Control,
    generation: AtomicU64,
    active: [AtomicU64; CPUS],
    tags: [AtomicU64; SPACES],
    numbers: Mutex<Numbers>,
    oracle: Mutex<Oracle>,
}

/// What each processor runs, and what its TLB holds: (number, space).
struct Oracle {
    running: [Option<(u64, usize)>; CPUS],
    tlb: [Vec<(u64, usize)>; CPUS],
}

impl Machine {
    fn new(control: Control) -> Machine {
        Machine {
            control,
            generation: AtomicU64::new(2),
            active: [AtomicU64::new(tag(2, 1)), AtomicU64::new(tag(2, 2))],
            tags: [
                AtomicU64::new(tag(2, 1)),
                AtomicU64::new(tag(2, 2)),
                AtomicU64::new(0),
                AtomicU64::new(tag(2, 3)),
                AtomicU64::new(tag(1, 1)),
            ],
            numbers: Mutex::new(Numbers {
                generation: 2,
                taken: [true; NUMBERS],
                next: 1,
                reserved: [0; CPUS],
                pending: [false; CPUS],
            }),
            oracle: Mutex::new(Oracle {
                running: [Some((1, S0)), Some((2, S1))],
                tlb: [vec![(1, S0), (3, DEAD)], vec![(2, S1), (3, DEAD)]],
            }),
        }
    }

    /// `asid::number_for`, then the oracle's look.
    fn install(&self, cpu: usize, space: usize) {
        // From here until its `TTBR0` write the processor runs no program:
        // what its old number can still do is a speculative walk, whose
        // entries are in its TLB below and must never be hit for another
        // space.
        self.oracle.lock().unwrap().running[cpu] = None;
        let held = self.tags[space].load(Ordering::Relaxed);
        let fast = held >> 8 == self.generation.load(Ordering::Relaxed)
            && number_of(held) != 0
            && if self.control == Control::SplitSwap {
                let was = self.active[cpu].load(Ordering::Relaxed);
                self.active[cpu].store(held, Ordering::Relaxed);
                was != 0
            } else {
                self.active[cpu].swap(held, Ordering::Relaxed) != 0
            };
        let number = if fast {
            number_of(held)
        } else {
            self.slow_path(cpu, space)
        };
        {
            let numbers = self.numbers.lock().unwrap();
            if numbers.pending[cpu] {
                assert_eq!(
                    number,
                    number_of(numbers.reserved[cpu]),
                    "a processor that has not flushed ran a number not reserved for it"
                );
            }
        }
        let mut oracle = self.oracle.lock().unwrap();
        if let Some(&(_, other)) = oracle.tlb[cpu].iter().find(|&&(n, _)| n == number) {
            assert_eq!(
                other, space,
                "a processor ran a number its TLB holds for another space"
            );
        }
        oracle.tlb[cpu].push((number, space));
        oracle.running[cpu] = Some((number, space));
        for (other, running) in oracle.running.iter().enumerate() {
            if other != cpu
                && let Some((n, s)) = running
            {
                assert!(
                    *n != number || *s == space,
                    "two processors ran one number for two spaces"
                );
            }
        }
    }

    /// `asid::slow_path`, with `Numbers::assign` restated.
    fn slow_path(&self, cpu: usize, space: usize) -> u64 {
        let mut numbers = self.numbers.lock().unwrap();
        let held = self.tags[space].load(Ordering::Relaxed);
        let given = if held >> 8 == numbers.generation && number_of(held) != 0 {
            held
        } else {
            self.assign(&mut numbers, held)
        };
        self.tags[space].store(given, Ordering::Relaxed);
        if core::mem::replace(&mut numbers.pending[cpu], false) {
            // The flush parks `TTBR0` first (ASID 0, `EPD0`): from here the
            // processor runs no number until its install.
            let mut oracle = self.oracle.lock().unwrap();
            oracle.tlb[cpu].clear();
            oracle.running[cpu] = None;
        }
        self.active[cpu].store(given, Ordering::Relaxed);
        number_of(given)
    }

    fn assign(&self, numbers: &mut Numbers, old: u64) -> u64 {
        if number_of(old) != 0 {
            let kept = tag(numbers.generation, number_of(old));
            let mut hit = false;
            for reserved in &mut numbers.reserved {
                let same = if self.control == Control::ReservedByNumber {
                    number_of(*reserved) == number_of(old) && *reserved != 0
                } else {
                    *reserved == old
                };
                if same {
                    *reserved = kept;
                    hit = true;
                }
            }
            if hit {
                return kept;
            }
            let number = number_of(old) as usize;
            if !numbers.taken[number] {
                numbers.taken[number] = true;
                return kept;
            }
        }
        let number = match free(numbers) {
            Some(number) => number,
            None => {
                self.roll_over(numbers);
                free(numbers).expect("a number free after a rollover")
            }
        };
        numbers.taken[number] = true;
        numbers.next = number + 1;
        tag(numbers.generation, number as u64)
    }

    fn roll_over(&self, numbers: &mut Numbers) {
        numbers.generation += 1;
        self.generation.store(numbers.generation, Ordering::Relaxed);
        numbers.taken = [false; NUMBERS];
        numbers.taken[0] = true;
        for cpu in 0..CPUS {
            let active = if self.control == Control::RolloverLeavesActive {
                self.active[cpu].load(Ordering::Relaxed)
            } else {
                self.active[cpu].swap(0, Ordering::Relaxed)
            };
            if active != 0 {
                numbers.reserved[cpu] = active;
            }
            let number = number_of(numbers.reserved[cpu]) as usize;
            numbers.taken[number] = true;
            numbers.pending[cpu] = true;
        }
        numbers.next = 1;
    }
}

/// The first free number from `next`, wrapping to 1.
fn free(numbers: &Numbers) -> Option<usize> {
    (numbers.next..NUMBERS)
        .chain(1..numbers.next.min(NUMBERS))
        .find(|&number| !numbers.taken[number])
}

fn fast_path_against_rollover(control: Control) {
    model(move || {
        let machine = Arc::new(Machine::new(control));
        let zero = {
            let machine = Arc::clone(&machine);
            thread::spawn(move || {
                machine.install(0, S0);
                machine.install(0, S2);
            })
        };
        let one = {
            let machine = Arc::clone(&machine);
            thread::spawn(move || {
                machine.install(1, S2);
                machine.install(1, OLD);
            })
        };
        zero.join().unwrap();
        one.join().unwrap();
    });
}

/// Verifies: L.armv7a.19
#[test]
fn a_fast_path_against_a_rollover() {
    fast_path_against_rollover(Control::None);
}

/// The control: the fast path's swap as a load and a store. A rollover's
/// clear can fall between them, so the processor's stale active tag lets
/// its next install skip the flush: it runs a number not reserved for it,
/// or one its TLB holds for another space.
#[test]
#[should_panic(expected = "a processor")]
fn control_the_fast_path_without_its_swap() {
    fast_path_against_rollover(Control::SplitSwap);
}

/// The control: a rollover that leaves every active tag as it found it, so
/// a processor's next install takes the fast path past its pending flush,
/// with the same two failures.
#[test]
#[should_panic(expected = "a processor")]
fn control_a_rollover_that_leaves_the_active_tags() {
    fast_path_against_rollover(Control::RolloverLeavesActive);
}

/// The control: a reservation matched by number, so a space whose tag is
/// two generations old takes the number another processor still runs for
/// another space.
#[test]
#[should_panic(expected = "two processors ran one number for two spaces")]
fn control_reservations_matched_by_number() {
    fast_path_against_rollover(Control::ReservedByNumber);
}
