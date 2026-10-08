//! Tests for the ASID allocator, and a model of the machine it serves.
//!
//! The unit tests hold the allocator to its rules one at a time. The model
//! (`asid_model_tlb_never_hits_another_space`) runs the allocator the way the
//! kernel does -- an install's fast and slow paths, an uninstall to ASID 0, a
//! rollover's flushes -- against processors whose TLBs keep every entry they
//! are given, leaf and table alike, until something removes it. It is the
//! TLB the architecture allows rather than the one QEMU has (QEMU empties
//! its TLB at every change of ASID, so no boot can see what this model
//! sees). Spaces are made, installed, used, shot down page by page, and
//! dropped without a shootdown, and their frames and tables go straight back
//! into use; every TLB hit must reach the running space's own frame through
//! its own table.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use super::*;

/// Processors in the model and the unit tests.
const CPUS: usize = 4;

/// The kernel's shootdowns on ARMv7-A reach every processor whatever a
/// space's set says (`TLBIMVAAIS`, `dsb ish`): `docs/OPAQUE-KERNEL.md`
/// §9.13, item 3. False makes them reach only the space's set, which a
/// processor leaves at its next switch.
const BROADCAST: bool = true;

/// Every number from 1 to 255 once per generation, never 0, through ten
/// generations of spaces that each live only for their first install; a tag
/// of the current generation is given back unchanged.
///
/// Verifies: L.mm.69
#[test]
fn asid_numbers_are_unique_in_a_generation() {
    let mut numbers: Numbers<CPUS> = Numbers::new();
    let mut seen = [false; NUMBERS];
    let mut generation = numbers.generation();
    for _ in 0..(NUMBERS - 1) * 10 {
        let given = numbers.assign(0, |_| 0).expect("a number");
        assert_eq!(
            numbers.assign(given.tag, |_| 0).expect("kept").tag,
            given.tag
        );
        if given.rolled_over {
            assert_eq!(numbers.generation(), generation + 1);
            generation += 1;
            seen = [false; NUMBERS];
        }
        let number = usize::from(number_of(given.tag));
        assert_ne!(number, usize::from(NO_SPACE), "number 0 was given");
        assert!(
            !seen[number],
            "number {number} given twice in generation {generation}"
        );
        seen[number] = true;
        assert_eq!(generation_of(given.tag), numbers.generation());
    }
    assert_eq!(numbers.generation(), 10);
}

/// A rollover comes exactly when every number is taken, reserves what each
/// processor is running (or kept from the rollover before), sets every
/// processor's flush pending, and the space a reservation names keeps its
/// number in the new generation; no older tag counts as current.
///
/// Verifies: L.mm.69
/// Verifies: L.mm.70
#[test]
fn asid_reserved_numbers_survive_a_rollover() {
    let mut numbers: Numbers<CPUS> = Numbers::new();
    let mut active = [0_u64; CPUS];
    let mut tags = Vec::new();
    for _ in 1..NUMBERS {
        let given = numbers.assign(0, |_| 0).expect("a number");
        assert!(!given.rolled_over, "rolled over with a number still free");
        tags.push(given.tag);
    }
    assert_eq!(numbers.next_free(), None);
    // Processors 0 and 2 run two of them; 1 and 3 run nothing.
    active[0] = tags[7];
    active[2] = tags[200];
    let old_generation = numbers.generation();
    let fresh = numbers
        .assign(0, |cpu| core::mem::replace(&mut active[cpu], 0))
        .expect("a number");
    assert!(fresh.rolled_over);
    assert_eq!(numbers.generation(), old_generation + 1);
    assert_eq!(active, [0; CPUS], "a rollover left an active tag");
    for cpu in 0..CPUS {
        assert!(
            numbers.is_pending(cpu),
            "processor {cpu}'s flush is not pending"
        );
    }
    assert_eq!(numbers.reserved(0), tags[7]);
    assert_eq!(numbers.reserved(2), tags[200]);
    assert!(!numbers.is_current(tags[7]));
    assert_ne!(number_of(fresh.tag), number_of(tags[7]));
    assert_ne!(number_of(fresh.tag), number_of(tags[200]));
    let kept = numbers.assign(tags[7], |_| 0).expect("kept");
    assert!(!kept.rolled_over);
    assert_eq!(number_of(kept.tag), number_of(tags[7]));
    assert_eq!(
        numbers.reserved(0),
        kept.tag,
        "the reservation did not move on"
    );
    // A second rollover with processor 2 idle since keeps 2's reservation.
    for _ in 0..NUMBERS {
        let given = numbers.assign(0, |cpu| core::mem::replace(&mut active[cpu], 0));
        if given.expect("a number").rolled_over {
            break;
        }
    }
    assert_eq!(number_of(numbers.reserved(2)), number_of(tags[200]));
    let still = numbers.assign(tags[200], |_| 0).expect("kept");
    assert_eq!(number_of(still.tag), number_of(tags[200]));
    assert!(numbers.take_pending(1));
    assert!(!numbers.take_pending(1));
}

/// The generation is refused, not wrapped, past its range.
///
/// Verifies: L.mm.70
#[test]
fn asid_generation_refuses_overflow() {
    let mut numbers: Numbers<CPUS> = Numbers::new();
    numbers.generation = MAX_GENERATION;
    for _ in 1..NUMBERS {
        let _ = numbers.assign(0, |_| 0).expect("a number");
    }
    let before = numbers.clone();
    assert_eq!(
        numbers.assign(0, |_| 0),
        Err(AsidError::GenerationExhausted)
    );
    assert_eq!(numbers.generation(), before.generation());
    assert_eq!(numbers.taken, before.taken);
}

/// A machine with as many processors as numbers, each running one, leaves
/// nothing free after a rollover: refused, never a number given twice.
///
/// Verifies: L.mm.70
#[test]
fn asid_rollover_with_every_number_reserved_is_refused() {
    let mut numbers: Numbers<NUMBERS> = Numbers::new();
    let mut active = [0_u64; NUMBERS];
    for slot in active.iter_mut().skip(1) {
        *slot = numbers.assign(0, |_| 0).expect("a number").tag;
    }
    assert_eq!(
        numbers.assign(0, |cpu| core::mem::replace(&mut active[cpu], 0)),
        Err(AsidError::NoneFree)
    );
}

/// The flush plan from `CTR` and `ID_MMFR1`: QEMU's and the Cortex-A7's
/// values, and the two cases that add an operation.
///
/// Verifies: L.armv7a.20
#[test]
fn asid_flush_plan_follows_the_core() {
    let a7 = FlushPlan::for_core(0x8444_8003, 0x4000_0000);
    assert_eq!(
        a7,
        FlushPlan {
            instruction_cache: false,
            predictor_every_install: false
        }
    );
    assert!(FlushPlan::for_core(0x8444_4003, 0x4000_0000).instruction_cache);
    assert!(!FlushPlan::for_core(0x8444_C003, 0x4000_0000).instruction_cache);
    assert!(FlushPlan::for_core(0x8444_8003, 0x1000_0000).predictor_every_install);
    assert!(!FlushPlan::for_core(0x8444_8003, 0x2000_0000).predictor_every_install);
    assert!(!FlushPlan::for_core(0x8444_8003, 0x0000_0000).predictor_every_install);
}

/// A small deterministic generator, so a failure names its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// A cached translation: a leaf, and the table the walk went through, both
/// as frames, tagged with the number the processor ran.
#[derive(Clone, Copy, Debug)]
struct Entry {
    asid: u8,
    va: u64,
    frame: u64,
    table: u64,
}

/// A modelled processor.
#[derive(Debug, Default)]
struct Cpu {
    running: Option<usize>,
    asid: u8,
    /// `TTBR0` as last written: the space's index plus one, 0 for none.
    base: usize,
    tlb: Vec<Entry>,
}

/// A modelled space: its tag, its pages (address to frame and table), its
/// set.
#[derive(Debug)]
struct Space {
    tag: u64,
    pages: BTreeMap<u64, (u64, u64)>,
    set: [bool; CPUS],
}

/// The model.
struct Machine {
    numbers: Numbers<CPUS>,
    active: [u64; CPUS],
    cpus: Vec<Cpu>,
    spaces: Vec<Option<Space>>,
    /// Frames in use, by the space and address (tables: address `u64::MAX`).
    owner: BTreeMap<u64, (usize, u64)>,
    free: Vec<u64>,
    next_frame: u64,
    rollovers: u64,
    flushes: u64,
    hits: u64,
}

/// The modelled user addresses: a few pages, two tables' worth.
const ADDRESSES: [u64; 6] = [0x1000, 0x2000, 0x3000, 0x20_1000, 0x20_2000, 0x20_3000];

impl Machine {
    fn new() -> Machine {
        Machine {
            numbers: Numbers::new(),
            active: [0; CPUS],
            cpus: (0..CPUS).map(|_| Cpu::default()).collect(),
            spaces: Vec::new(),
            owner: BTreeMap::new(),
            free: Vec::new(),
            next_frame: 1,
            rollovers: 0,
            flushes: 0,
            hits: 0,
        }
    }

    fn frame(&mut self, space: usize, va: u64) -> u64 {
        let frame = self.free.pop().unwrap_or_else(|| {
            self.next_frame += 1;
            self.next_frame
        });
        assert!(self.owner.insert(frame, (space, va)).is_none());
        frame
    }

    fn release(&mut self, frame: u64) {
        assert!(self.owner.remove(&frame).is_some());
        self.free.push(frame);
    }

    fn new_space(&mut self, rng: &mut Rng) {
        let index = self.spaces.len();
        let mut pages = BTreeMap::new();
        let tables = [self.frame(index, u64::MAX), self.frame(index, u64::MAX)];
        for &va in &ADDRESSES {
            if rng.below(3) != 0 {
                let frame = self.frame(index, va);
                let _ = pages.insert(va, (frame, tables[usize::from(va >= 0x20_0000)]));
            }
        }
        for table in tables {
            if !pages.values().any(|&(_, used)| used == table) {
                self.release(table);
            }
        }
        self.spaces.push(Some(Space {
            tag: 0,
            pages,
            set: [false; CPUS],
        }));
    }

    /// The kernel's install, its fast path and its slow path.
    fn install(&mut self, cpu: usize, index: usize) {
        let tag = self.spaces[index].as_ref().expect("live").tag;
        let fast =
            self.numbers.is_current(tag) && core::mem::replace(&mut self.active[cpu], tag) != 0;
        if !fast {
            let active = &mut self.active;
            let assigned = self
                .numbers
                .assign(tag, |other| core::mem::replace(&mut active[other], 0))
                .expect("a number");
            if assigned.rolled_over {
                self.rollovers += 1;
            }
            self.spaces[index].as_mut().expect("live").tag = assigned.tag;
            if self.numbers.take_pending(cpu) {
                self.cpus[cpu].tlb.clear();
                self.flushes += 1;
            }
            self.active[cpu] = assigned.tag;
        }
        let tag = self.spaces[index].as_ref().expect("live").tag;
        let number = number_of(tag);
        assert_ne!(number, NO_SPACE);
        if let Some(previous) = self.cpus[cpu].running
            && previous != index
            && let Some(space) = self.spaces[previous].as_mut()
        {
            space.set[cpu] = false;
        }
        self.spaces[index].as_mut().expect("live").set[cpu] = true;
        // B3.10.2: a new base needs a TLB invalidation unless the ASID
        // changes with it, and an install invalidates nothing.
        assert!(
            self.cpus[cpu].asid != number || self.cpus[cpu].base == index + 1,
            "processor {cpu} changed its base and kept number {number}"
        );
        self.cpus[cpu].running = Some(index);
        self.cpus[cpu].asid = number;
        self.cpus[cpu].base = index + 1;
        self.check_numbers_mean_one_space();
    }

    fn uninstall(&mut self, cpu: usize) {
        if let Some(previous) = self.cpus[cpu].running.take()
            && let Some(space) = self.spaces[previous].as_mut()
        {
            space.set[cpu] = false;
        }
        self.cpus[cpu].asid = NO_SPACE;
        self.cpus[cpu].base = 0;
    }

    /// A use of `va`, or a walk speculated through it: a hit must reach the
    /// running space's own frame through its own table.
    fn touch(&mut self, cpu: usize, va: u64) {
        let Some(index) = self.cpus[cpu].running else {
            assert!(
                self.cpus[cpu]
                    .tlb
                    .iter()
                    .all(|entry| entry.asid != NO_SPACE),
                "an entry is tagged ASID 0"
            );
            return;
        };
        let asid = self.cpus[cpu].asid;
        let hit = self.cpus[cpu]
            .tlb
            .iter()
            .find(|entry| entry.asid == asid && entry.va == va)
            .copied();
        let space = self.spaces[index]
            .as_ref()
            .expect("a running space is live");
        match hit {
            Some(entry) => {
                self.hits += 1;
                assert_eq!(
                    space.pages.get(&va).copied(),
                    Some((entry.frame, entry.table)),
                    "processor {cpu} hit an entry that is not its space's: {entry:?}"
                );
                assert_eq!(self.owner.get(&entry.frame), Some(&(index, va)));
                assert_eq!(self.owner.get(&entry.table), Some(&(index, u64::MAX)));
            }
            None => {
                if let Some(&(frame, table)) = space.pages.get(&va) {
                    self.cpus[cpu].tlb.push(Entry {
                        asid,
                        va,
                        frame,
                        table,
                    });
                }
            }
        }
    }

    /// An unmap: the entry out, the shootdown, then the frame back.
    fn unmap(&mut self, index: usize, va: u64) {
        let Some(space) = self.spaces[index].as_mut() else {
            return;
        };
        let Some((frame, table)) = space.pages.remove(&va) else {
            return;
        };
        let set = space.set;
        let table_empty = !space.pages.values().any(|&(_, used)| used == table);
        for (cpu, processor) in self.cpus.iter_mut().enumerate() {
            if BROADCAST || set[cpu] {
                processor.tlb.retain(|entry| entry.va != va);
            }
        }
        self.release(frame);
        if table_empty {
            self.release(table);
        }
    }

    /// A drop: no processor runs the space, and its frames and tables go back
    /// at once, with no shootdown.
    fn drop_space(&mut self, index: usize) {
        if self.cpus.iter().any(|cpu| cpu.running == Some(index)) {
            return;
        }
        let Some(space) = self.spaces[index].take() else {
            return;
        };
        let mut tables = Vec::new();
        for (_, (frame, table)) in space.pages {
            self.release(frame);
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
        for table in tables {
            self.release(table);
        }
    }

    /// B3.9.1: a number means one space on every processor at once.
    fn check_numbers_mean_one_space(&self) {
        for (one, a) in self.cpus.iter().enumerate() {
            for b in self.cpus.iter().skip(one + 1) {
                if a.asid != NO_SPACE && a.asid == b.asid {
                    assert_eq!(a.running, b.running, "one number, two spaces at once");
                }
            }
        }
        let mut current = BTreeMap::new();
        for (index, space) in self.spaces.iter().enumerate() {
            if let Some(space) = space
                && self.numbers.is_current(space.tag)
            {
                assert!(
                    current.insert(number_of(space.tag), index).is_none(),
                    "two live spaces hold one number in one generation"
                );
            }
        }
    }
}

/// The allocator, run as the kernel runs it, never lets a processor's TLB
/// give it another space's translation or a freed frame, across drops with
/// no shootdown, frame and table reuse, broadcast shootdowns and rollovers;
/// and a number means one space on every processor at once.
///
/// Verifies: L.mm.69
/// Verifies: L.mm.70
/// Verifies: L.armv7a.16
/// Verifies: L.armv7a.18
#[test]
fn asid_model_tlb_never_hits_another_space() {
    for seed in 1..=8_u64 {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed);
        let mut machine = Machine::new();
        for _ in 0..40_000 {
            match rng.below(100) {
                0..=7 => machine.new_space(&mut rng),
                8..=37 => {
                    let live: Vec<usize> = (0..machine.spaces.len())
                        .filter(|&index| machine.spaces[index].is_some())
                        .collect();
                    if let Some(&index) = live.get(rng.below(live.len().max(1) as u64) as usize) {
                        machine.install(rng.below(CPUS as u64) as usize, index);
                    }
                }
                38..=42 => machine.uninstall(rng.below(CPUS as u64) as usize),
                43..=84 => {
                    let va = ADDRESSES[rng.below(ADDRESSES.len() as u64) as usize];
                    machine.touch(rng.below(CPUS as u64) as usize, va);
                }
                85..=89 => {
                    if !machine.spaces.is_empty() {
                        let index = rng.below(machine.spaces.len() as u64) as usize;
                        let va = ADDRESSES[rng.below(ADDRESSES.len() as u64) as usize];
                        machine.unmap(index, va);
                    }
                }
                _ => {
                    if !machine.spaces.is_empty() {
                        let index = rng.below(machine.spaces.len() as u64) as usize;
                        machine.drop_space(index);
                    }
                }
            }
        }
        assert!(
            machine.rollovers >= 5,
            "seed {seed}: only {} rollovers",
            machine.rollovers
        );
        assert!(
            machine.hits > 1_000,
            "seed {seed}: only {} hits",
            machine.hits
        );
        assert!(
            machine.flushes >= machine.rollovers,
            "seed {seed}: fewer flushes than rollovers"
        );
    }
}
