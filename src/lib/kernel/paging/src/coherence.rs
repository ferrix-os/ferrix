//! Table writes a walker that does not snoop the processor's caches must find
//! in memory.
//!
//! An IOMMU walks its tables -- root and context entries, second-level
//! tables -- by reading memory. A unit whose walk snoops the caches (VT-d's
//! `ECAP.C` set) sees what a processor wrote through its cached direct map.
//! One that does not (`ECAP.C` clear) reads memory itself, and a line still
//! dirty in a processor's cache is a line it does not see: a cleared entry
//! still reads as present after its invalidation, so a device keeps reaching
//! the frame it named, and a fresh table over stale memory reads as present
//! entries (finding F-58).
//!
//! [`Unpublished`] is the record of what has been written and not yet
//! cleaned to memory. Each entry write is noted in it; [`Unpublished::publish`]
//! cleans every line noted and empties it; and whatever publishes the writes
//! to the unit -- an invalidation, or a map returning to the device's driver
//! -- requires [`Unpublished::is_published`] first. A fresh table is cleaned
//! whole when it is noted, because it must be in memory before any entry
//! links it. On a unit that snoops, nothing is noted and nothing cleaned.
//!
//! [`Walked`] puts the mapper's own writes through the record, so the
//! second-level entries and intermediate tables the mapper writes are cleaned
//! like the ones the unit's driver writes itself.

use crate::{PAGE_SIZE, PhysAddr, PhysMem};

/// Cleaning a range of physical memory out of the processor's caches to
/// where a walker that does not snoop reads it, done when it returns.
pub trait Clean {
    /// Clean the `len` bytes at `at`, and wait until they are in memory.
    fn clean(&mut self, at: PhysAddr, len: u64);
}

/// Writes noted and not cleaned that one record holds before it cleans
/// them early. More than any one change of a unit's tables makes: a map
/// writes at most one entry per level, an attach three.
const SPANS: usize = 8;

/// The writes a non-snooping walker could read that have not been cleaned
/// to memory yet. See the module documentation.
#[derive(Clone, Copy, Debug)]
pub struct Unpublished {
    /// The walker snoops: nothing is noted or cleaned.
    coherent: bool,
    /// Each write not yet cleaned: where, and how many bytes.
    spans: [(u64, u64); SPANS],
    /// How many of `spans` are in use.
    held: usize,
    /// Entry writes noted, over the record's life.
    entries: u64,
    /// Fresh tables cleaned whole, over the record's life.
    tables: u64,
}

impl Unpublished {
    /// A record for a walker that snoops (`coherent`) or one that does not.
    #[must_use]
    pub const fn new(coherent: bool) -> Self {
        Unpublished {
            coherent,
            spans: [(0, 0); SPANS],
            held: 0,
            entries: 0,
            tables: 0,
        }
    }

    /// Note the `len` bytes of entry at `at` as written. A record already
    /// full is cleaned first, so nothing written is ever forgotten.
    pub fn wrote(&mut self, at: PhysAddr, len: u64, clean: &mut impl Clean) {
        if self.coherent {
            return;
        }
        self.note(at, len, clean);
        self.entries += 1;
    }

    /// Note the zeroed table frame at `at`, and clean it whole now: it has
    /// to be in memory before an entry that links it can be, since the
    /// processor's cache may write that entry back at any moment.
    pub fn fresh_table(&mut self, at: PhysAddr, clean: &mut impl Clean) {
        if self.coherent {
            return;
        }
        self.note(at, PAGE_SIZE, clean);
        self.tables += 1;
        self.publish(clean);
    }

    /// Hold the `len` bytes at `at` until the next publish, cleaning what
    /// is held first if there is no room.
    fn note(&mut self, at: PhysAddr, len: u64, clean: &mut impl Clean) {
        if self.held == SPANS {
            self.publish(clean);
        }
        if let Some(span) = self.spans.get_mut(self.held) {
            *span = (at.0, len);
            self.held += 1;
        } else {
            // Not reached: the publish above emptied the record. Cleaned
            // now rather than forgotten.
            clean.clean(at, len);
        }
    }

    /// Clean every write noted, and forget them.
    pub fn publish(&mut self, clean: &mut impl Clean) {
        for &(at, len) in self.spans.iter().take(self.held) {
            clean.clean(PhysAddr(at), len);
        }
        self.held = 0;
    }

    /// Whether every write noted has been cleaned: what an invalidation, or
    /// anything else that lets the unit read the writes, requires.
    #[must_use]
    pub const fn is_published(&self) -> bool {
        self.held == 0
    }

    /// Whether the walker snoops, so nothing is noted.
    #[must_use]
    pub const fn is_coherent(&self) -> bool {
        self.coherent
    }

    /// Entry writes noted over the record's life, and fresh tables cleaned.
    #[must_use]
    pub const fn counts(&self) -> (u64, u64) {
        (self.entries, self.tables)
    }
}

/// A [`PhysMem`] whose every write and fresh table is noted in an
/// [`Unpublished`], for tables a walker that may not snoop reads.
#[derive(Debug)]
pub struct Walked<'a, M: PhysMem, C: Clean> {
    /// The memory written.
    memory: &'a mut M,
    /// What has been written and not cleaned.
    writes: &'a mut Unpublished,
    /// How a range is cleaned.
    clean: C,
}

impl<'a, M: PhysMem, C: Clean> Walked<'a, M, C> {
    /// `memory`, with its writes noted in `writes` and cleaned by `clean`.
    pub fn new(memory: &'a mut M, writes: &'a mut Unpublished, clean: C) -> Self {
        Walked {
            memory,
            writes,
            clean,
        }
    }
}

// SAFETY: every access is `memory`'s, whose implementation guarantees what
// the trait asks; noting a write or cleaning a range changes no byte.
unsafe impl<M: PhysMem, C: Clean> PhysMem for Walked<'_, M, C> {
    fn read(&self, at: PhysAddr) -> u64 {
        self.memory.read(at)
    }

    fn write(&mut self, at: PhysAddr, value: u64) {
        self.memory.write(at, value);
        self.writes.wrote(at, 8, &mut self.clean);
    }

    fn allocate_table(&mut self) -> Option<PhysAddr> {
        let table = self.memory.allocate_table()?;
        self.writes.fresh_table(table, &mut self.clean);
        Some(table)
    }
}
