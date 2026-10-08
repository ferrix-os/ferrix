//! Address space identifiers: which 8-bit number each address space is
//! tagged with in the TLB, for the whole machine at once.
//!
//! The arithmetic half of ARMv7-A's ASIDs (`docs/OPAQUE-KERNEL.md` §9.13).
//! The kernel keeps a [`Numbers`] under one spin lock and supplies the one
//! thing this type cannot hold, each processor's *active* tag, which the
//! fast path of an install swaps without the lock.
//!
//! # The rules this type keeps
//!
//! The ARM architecture requires every ASID to mean the same space on every
//! processor of the inner shareable domain (ARM DDI 0406C.d B3.9.1), so there
//! is one allocator for the machine, in the shape of Linux's
//! `arch/arm/mm/context.c`:
//!
//! * A space holds a *tag*, `generation << 8 | number`, or 0 before it has
//!   one. Number 0 is never given: it is what a processor runs with no space
//!   installed.
//! * Within a generation a number is given to at most one space, and it is
//!   never given back, whether the space lives or dies.
//! * When no number is free, the allocator *rolls over*: a new generation, an
//!   empty map, and each processor's active tag (or, if it has run nothing
//!   since the last rollover, its earlier reservation) *reserved*, so that
//!   the number the processor is running keeps meaning the same space in the
//!   new generation. Every processor's flush is then pending, and a processor
//!   writes no number of the new generation before it has flushed its TLB.
//! * A space whose tag is of an old generation keeps its number if a
//!   processor reserved it, or if it is free; otherwise it takes the next free
//!   number.
//!
//! The generation only grows, and is refused past [`MAX_GENERATION`] rather
//! than wrapped: a wrapped generation would make an ancient tag current.

/// How many numbers there are: 8-bit ASIDs.
pub const NUMBERS: usize = 256;

/// The number no space is given.
pub const NO_SPACE: u8 = 0;

/// The tag's number occupies its low eight bits.
const NUMBER_BITS: u32 = 8;

/// The largest generation a tag can carry.
pub const MAX_GENERATION: u64 = u64::MAX >> NUMBER_BITS;

/// Words of the map of numbers given.
const MAP_WORDS: usize = NUMBERS / 64;

/// The tag for `number` in `generation`.
#[must_use]
pub const fn tag(generation: u64, number: u8) -> u64 {
    (generation << NUMBER_BITS) | number as u64
}

/// The number a tag carries.
#[must_use]
pub const fn number_of(tag: u64) -> u8 {
    (tag & 0xFF) as u8
}

/// The generation a tag carries.
#[must_use]
pub const fn generation_of(tag: u64) -> u64 {
    tag >> NUMBER_BITS
}

/// Why a number could not be given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsidError {
    /// The generation would pass [`MAX_GENERATION`].
    GenerationExhausted,
    /// Every number is reserved by a processor even after a rollover: a
    /// machine with 255 processors or more running a space each, which no
    /// machine this allocator serves can be.
    NoneFree,
}

/// A number given to a space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Assigned {
    /// The space's new tag.
    pub tag: u64,
    /// Whether giving it rolled the allocator over.
    pub rolled_over: bool,
}

/// The machine's numbers, for `CPUS` processors.
#[derive(Clone, Debug)]
pub struct Numbers<const CPUS: usize> {
    /// The current generation, from 1.
    generation: u64,
    /// The numbers given in this generation, number 0 always among them.
    taken: [u64; MAP_WORDS],
    /// Where the search for a free number starts.
    next: usize,
    /// Each processor's reserved tag, 0 for none.
    reserved: [u64; CPUS],
    /// Each processor's flush, pending since a rollover.
    pending: [bool; CPUS],
}

impl<const CPUS: usize> Default for Numbers<CPUS> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const CPUS: usize> Numbers<CPUS> {
    /// Generation 1, no number given, nothing reserved, no flush pending: the
    /// state of a machine whose processors have each emptied their TLB once
    /// since reset and run no number since.
    #[must_use]
    pub const fn new() -> Self {
        Numbers {
            generation: 1,
            taken: [1, 0, 0, 0],
            next: 1,
            reserved: [0; CPUS],
            pending: [false; CPUS],
        }
    }

    /// The current generation.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether `tag` is of the current generation and names a number.
    #[must_use]
    pub const fn is_current(&self, tag: u64) -> bool {
        generation_of(tag) == self.generation && number_of(tag) != NO_SPACE
    }

    /// Whether processor `cpu` must flush before it writes a number of this
    /// generation, clearing the request: what its first slow path after a
    /// rollover asks.
    pub fn take_pending(&mut self, cpu: usize) -> bool {
        self.pending
            .get_mut(cpu)
            .is_some_and(|pending| core::mem::replace(pending, false))
    }

    /// Whether processor `cpu`'s flush is pending, without clearing it.
    #[must_use]
    pub fn is_pending(&self, cpu: usize) -> bool {
        self.pending.get(cpu).copied().unwrap_or(false)
    }

    /// Processor `cpu`'s reserved tag, 0 for none.
    #[must_use]
    pub fn reserved(&self, cpu: usize) -> u64 {
        self.reserved.get(cpu).copied().unwrap_or(0)
    }

    /// The number the next space with no number to keep would be given, if
    /// no rollover came first: for a check that wants a number reused.
    #[must_use]
    pub fn next_free(&self) -> Option<u8> {
        self.find_free()
    }

    /// The tag for a space whose tag is `old`: `old` itself if it is current,
    /// else a number of the current generation, rolling over if none is free.
    ///
    /// `swap_active(cpu)` swaps processor `cpu`'s active tag with 0 and
    /// answers what it held; it is called once for every processor, and only
    /// at a rollover.
    ///
    /// # Errors
    ///
    /// [`AsidError::GenerationExhausted`] if a rollover would pass
    /// [`MAX_GENERATION`]; nothing changes. [`AsidError::NoneFree`] if a
    /// rollover leaves every number reserved.
    pub fn assign(
        &mut self,
        old: u64,
        swap_active: impl FnMut(usize) -> u64,
    ) -> Result<Assigned, AsidError> {
        if self.is_current(old) {
            return Ok(Assigned {
                tag: old,
                rolled_over: false,
            });
        }
        if number_of(old) != NO_SPACE {
            let kept = tag(self.generation, number_of(old));
            // Reserved at a rollover: running on some processor now, so the
            // number must keep meaning this space. Every reservation of it
            // moves on, or a later rollover would miss it.
            let mut hit = false;
            for reserved in &mut self.reserved {
                if *reserved == old {
                    *reserved = kept;
                    hit = true;
                }
            }
            if hit {
                return Ok(Assigned {
                    tag: kept,
                    rolled_over: false,
                });
            }
            if self.take(number_of(old)) {
                return Ok(Assigned {
                    tag: kept,
                    rolled_over: false,
                });
            }
        }
        let (number, rolled_over) = match self.find_free() {
            Some(number) => (number, false),
            None => {
                self.roll_over(swap_active)?;
                // The rollover reserves at most one number a processor, so
                // with fewer than 255 processors one is always free.
                (self.find_free().ok_or(AsidError::NoneFree)?, true)
            }
        };
        let _ = self.take(number);
        self.next = usize::from(number) + 1;
        Ok(Assigned {
            tag: tag(self.generation, number),
            rolled_over,
        })
    }

    /// Mark `number` given; `true` if it was free.
    fn take(&mut self, number: u8) -> bool {
        let (word, bit) = (usize::from(number) / 64, u64::from(number) % 64);
        match self.taken.get_mut(word) {
            Some(bits) if *bits & (1 << bit) == 0 => {
                *bits |= 1 << bit;
                true
            }
            _ => false,
        }
    }

    /// Whether `number` is given in this generation.
    fn is_taken(&self, number: usize) -> bool {
        self.taken
            .get(number / 64)
            .is_none_or(|bits| bits & (1 << (number % 64)) != 0)
    }

    /// The first free number from `next` on, wrapping to 1.
    fn find_free(&self) -> Option<u8> {
        (self.next..NUMBERS)
            .chain(1..self.next.min(NUMBERS))
            .find(|&number| !self.is_taken(number))
            .and_then(|number| u8::try_from(number).ok())
    }

    /// Start a generation: see the module's rules.
    fn roll_over(&mut self, mut swap_active: impl FnMut(usize) -> u64) -> Result<(), AsidError> {
        let generation = self
            .generation
            .checked_add(1)
            .filter(|&generation| generation <= MAX_GENERATION)
            .ok_or(AsidError::GenerationExhausted)?;
        self.generation = generation;
        self.taken = [1, 0, 0, 0];
        for cpu in 0..CPUS {
            let active = swap_active(cpu);
            if let Some(reserved) = self.reserved.get_mut(cpu) {
                if active != 0 {
                    *reserved = active;
                }
                let number = number_of(*reserved);
                let _ = self.take(number);
            }
            if let Some(pending) = self.pending.get_mut(cpu) {
                *pending = true;
            }
        }
        self.next = 1;
        Ok(())
    }
}

/// What a processor's flush after a rollover must add to `TLBIALL`, decided
/// from its identification registers (`docs/OPAQUE-KERNEL.md` §9.13, item 2
/// and item 9).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushPlan {
    /// `ICIALLU` in the flush: the instruction cache is ASID-tagged VIVT
    /// (`CTR.L1Ip` 0b01, DDI 0406C.d B4.1.42).
    pub instruction_cache: bool,
    /// `BPIALL` at every install, not only in the flush: the predictor needs
    /// maintenance at every change of `ContextID` (`ID_MMFR1.BPred` 0b0001,
    /// B4.1.90; 0b0000 is a core with no predictor).
    pub predictor_every_install: bool,
}

impl FlushPlan {
    /// The plan for a core with `ctr` and `id_mmfr1`.
    #[must_use]
    pub const fn for_core(ctr: u32, id_mmfr1: u32) -> FlushPlan {
        let l1ip = (ctr >> 14) & 0b11;
        let bpred = id_mmfr1 >> 28;
        FlushPlan {
            instruction_cache: l1ip == 0b01,
            predictor_every_install: bpred == 0b0001,
        }
    }
}

#[cfg(test)]
mod tests;
