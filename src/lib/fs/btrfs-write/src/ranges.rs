//! Sets of byte ranges: free space, pinned space, what the free-space tree says.
//!
//! Every range set is kept in one canonical form — disjoint, sorted, and with
//! touching ranges merged — so two sets holding the same bytes compare equal,
//! and a set turned into free-space items gives exactly the items btrfs
//! itself would record: one per maximal run.

use alloc::collections::BTreeMap;
use core::ops::Bound;

use crate::{Result, fallible};

/// A set of disjoint, non-adjacent `[start, end)` byte ranges.
///
/// Not `Clone` outside the tests: a copy allocates, and is
/// [`RangeSet::try_clone`].
#[derive(Debug, Default, PartialEq, Eq)]
#[cfg_attr(test, derive(Clone))]
pub struct RangeSet {
    /// Start to end, exclusive.
    map: BTreeMap<u64, u64>,
}

impl RangeSet {
    /// An empty set.
    #[must_use]
    pub const fn new() -> Self {
        RangeSet {
            map: BTreeMap::new(),
        }
    }

    /// Whether the set holds no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// How many maximal runs the set holds.
    #[must_use]
    pub fn runs(&self) -> usize {
        self.map.len()
    }

    /// Total bytes in the set.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.map
            .iter()
            .fold(0u64, |sum, (start, end)| sum.saturating_add(end - start))
    }

    /// The runs, as `(start, length)`, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = (u64, u64)> + '_ {
        self.map.iter().map(|(&start, &end)| (start, end - start))
    }

    /// Whether `[start, start + len)` is one of the set's runs exactly.
    #[must_use]
    pub fn has_run(&self, start: u64, len: u64) -> bool {
        start
            .checked_add(len)
            .is_some_and(|end| self.map.get(&start) == Some(&end))
    }

    /// Remove everything.
    pub fn clear(&mut self) {
        self.map.clear();
    }

    /// The run containing `at`, as `(start, end)`.
    fn run_containing(&self, at: u64) -> Option<(u64, u64)> {
        let (&start, &end) = self.map.range(..=at).next_back()?;
        (at < end).then_some((start, end))
    }

    /// Whether any byte of `[start, start + len)` is in the set.
    #[must_use]
    pub fn overlaps(&self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else {
            return true;
        };
        if len == 0 {
            return false;
        }
        if self.run_containing(start).is_some() {
            return true;
        }
        self.map.range(start..end).next().is_some()
    }

    /// Whether every byte of `[start, start + len)` is in the set.
    #[must_use]
    pub fn contains(&self, start: u64, len: u64) -> bool {
        let Some(end) = start.checked_add(len) else {
            return false;
        };
        len == 0
            || self
                .run_containing(start)
                .is_some_and(|(_, run_end)| end <= run_end)
    }

    /// Add `[start, start + len)`, merging with runs it touches. `false`, and
    /// no change, if any of it is already in the set: adding a byte twice is
    /// always a bookkeeping error somewhere, never something to absorb.
    ///
    /// [`crate::Error::OutOfMemory`], and no change, when a run had to be made and
    /// could not be: a merge with the run before edits that run in place, so
    /// only a range starting a run of its own allocates.
    pub fn try_insert(&mut self, start: u64, len: u64) -> Result<bool> {
        let Some(end) = start.checked_add(len) else {
            return Ok(false);
        };
        if len == 0 {
            return Ok(true);
        }
        if self.overlaps(start, len) {
            return Ok(false);
        }
        let after_end = self.map.get(&end).copied();
        let before = self
            .map
            .range_mut(..start)
            .next_back()
            .filter(|(_, before_end)| **before_end == start);
        if let Some((_, before_end)) = before {
            *before_end = after_end.unwrap_or(end);
        } else {
            let _ = fallible::insert(&mut self.map, start, after_end.unwrap_or(end))?;
        }
        if after_end.is_some() {
            let _ = self.map.remove(&end);
        }
        Ok(true)
    }

    /// Take `[start, start + len)` out of the set, splitting the run holding
    /// it. `false`, and no change, unless all of it is in one run.
    ///
    /// [`crate::Error::OutOfMemory`], and no change, when the run had to be split or
    /// its start moved and the new run could not be made.
    pub fn try_remove(&mut self, start: u64, len: u64) -> Result<bool> {
        let Some(end) = start.checked_add(len) else {
            return Ok(false);
        };
        if len == 0 {
            return Ok(true);
        }
        let Some((run_start, run_end)) = self.run_containing(start) else {
            return Ok(false);
        };
        if end > run_end {
            return Ok(false);
        }
        // The part after the range is the one that needs a key of its own;
        // it is made first, so a failure leaves the set as it was.
        if end < run_end {
            let _ = fallible::insert(&mut self.map, end, run_end)?;
        }
        if run_start < start {
            if let Some(kept) = self.map.get_mut(&run_start) {
                *kept = start;
            }
        } else {
            let _ = self.map.remove(&run_start);
        }
        Ok(true)
    }

    /// Add every run of `other`, which must not overlap this set. `false` if
    /// some of it did.
    ///
    /// Not atomic: on [`crate::Error::OutOfMemory`] the runs before the one that
    /// failed are in. Every caller either works on a copy or is inside an
    /// edit, which the failure aborts.
    pub fn try_absorb(&mut self, other: &RangeSet) -> Result<bool> {
        let mut all = true;
        for (start, len) in other.iter() {
            all &= self.try_insert(start, len)?;
        }
        Ok(all)
    }

    /// Add `[start, start + len)` whether or not some of it is already in
    /// the set: a union, for sets built from ranges that may repeat.
    ///
    /// [`crate::Error::OutOfMemory`], and no change, when the union starts a run of
    /// its own and that could not be made.
    pub fn try_add(&mut self, start: u64, len: u64) -> Result<()> {
        let Some(end) = start.checked_add(len) else {
            return Ok(());
        };
        if len == 0 {
            return Ok(());
        }
        // The runs it touches are consecutive, from the last one starting at
        // or before `end` back to the first ending at or after `start`.
        let (mut first, mut last) = (start, end);
        for (&run_start, &run_end) in self
            .map
            .range(..=end)
            .rev()
            .take_while(|&(_, &run_end)| run_end >= start)
        {
            first = first.min(run_start);
            last = last.max(run_end);
        }
        if let Some(kept) = self.map.get_mut(&first) {
            *kept = last;
        } else {
            let _ = fallible::insert(&mut self.map, first, last)?;
        }
        // Every other run it touched starts after `first` and at most at
        // `end`.
        while let Some(&run_start) = self
            .map
            .range((Bound::Excluded(first), Bound::Included(end)))
            .map(|(start, _)| start)
            .next()
        {
            let _ = self.map.remove(&run_start);
        }
        Ok(())
    }

    /// Take every byte `other` also holds out of this set, and return them.
    ///
    /// Not atomic: on [`crate::Error::OutOfMemory`] some of the shared bytes may be
    /// out of this set already. Every caller either works on a copy or is
    /// inside an edit, which the failure aborts.
    pub fn try_extract(&mut self, other: &RangeSet) -> Result<RangeSet> {
        let mut taken = RangeSet::new();
        for (start, len) in other.iter() {
            let end = start.saturating_add(len);
            let first = self.run_containing(start).map_or(start, |(run, _)| run);
            for (&run_start, &run_end) in self.map.range(first..end) {
                let from = run_start.max(start);
                let to = run_end.min(end);
                if from < to {
                    let _ = taken.try_insert(from, to - from)?;
                }
            }
        }
        for (start, len) in taken.iter() {
            let _ = self.try_remove(start, len)?;
        }
        Ok(taken)
    }

    /// A copy of the set, one run at a time.
    pub fn try_clone(&self) -> Result<RangeSet> {
        Ok(RangeSet {
            map: fallible::copy_map(&self.map)?,
        })
    }

    /// The lowest address at or after `from` where `len` bytes aligned to
    /// `align` fit inside one run and, when `boundary` is non-zero, do not
    /// cross a multiple of it. Falls back to the lowest such address before
    /// `from` when there is none after, so a cursor can wrap.
    #[must_use]
    pub fn first_fit(&self, len: u64, align: u64, boundary: u64, from: u64) -> Option<u64> {
        let fits =
            |(&start, &end): (&u64, &u64)| fit_in(start.max(from), end, len, align, boundary);
        let after = self
            .run_containing(from)
            .and_then(|(start, end)| fit_in(start.max(from), end, len, align, boundary))
            .or_else(|| self.map.range(from..).find_map(fits));
        after.or_else(|| {
            self.map
                .iter()
                .find_map(|(&start, &end)| fit_in(start, end, len, align, boundary))
        })
    }

    /// The longest prefix, up to `want` bytes and at least `min`, of the
    /// first run at or after `from` (wrapping) that holds `min` aligned
    /// bytes. Returns `(start, len)`, with `len` a multiple of `align`.
    #[must_use]
    pub fn first_prefix(&self, want: u64, min: u64, align: u64, from: u64) -> Option<(u64, u64)> {
        let take = |start: u64, end: u64| -> Option<(u64, u64)> {
            let start = start.checked_next_multiple_of(align.max(1))?;
            let room = end.checked_sub(start)?;
            let room = room - room % align.max(1);
            (room >= min && room > 0).then_some((start, room.min(want)))
        };
        let after = self
            .run_containing(from)
            .and_then(|(_, end)| take(from, end))
            .or_else(|| self.map.range(from..).find_map(|(&s, &e)| take(s, e)));
        after.or_else(|| self.map.iter().find_map(|(&s, &e)| take(s, e)))
    }
}

/// The first aligned start in `[start, end)` where `len` bytes fit without
/// crossing a multiple of `boundary` (when non-zero).
fn fit_in(start: u64, end: u64, len: u64, align: u64, boundary: u64) -> Option<u64> {
    let mut at = start.checked_next_multiple_of(align.max(1))?;
    loop {
        let stop = at.checked_add(len)?;
        if stop > end {
            return None;
        }
        if boundary == 0 || at / boundary == (stop - 1) / boundary {
            return Some(at);
        }
        at = (at / boundary)
            .checked_add(1)?
            .checked_mul(boundary)?
            .checked_next_multiple_of(align.max(1))?;
    }
}

/// The infallible forms the tests were written against: on the host, out of
/// memory is not something a test of the set's arithmetic is about.
#[cfg(test)]
impl RangeSet {
    /// [`RangeSet::try_insert`].
    pub fn insert(&mut self, start: u64, len: u64) -> bool {
        self.try_insert(start, len).expect("host memory")
    }

    /// [`RangeSet::try_remove`].
    pub fn remove(&mut self, start: u64, len: u64) -> bool {
        self.try_remove(start, len).expect("host memory")
    }

    /// [`RangeSet::try_absorb`].
    pub fn absorb(&mut self, other: &RangeSet) -> bool {
        self.try_absorb(other).expect("host memory")
    }

    /// [`RangeSet::try_add`].
    pub fn add(&mut self, start: u64, len: u64) {
        self.try_add(start, len).expect("host memory");
    }

    /// [`RangeSet::try_extract`].
    pub fn extract(&mut self, other: &RangeSet) -> RangeSet {
        self.try_extract(other).expect("host memory")
    }
}

#[cfg(test)]
mod tests;
