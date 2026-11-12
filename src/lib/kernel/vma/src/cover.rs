//! How many times each page index of one object is covered by one address
//! space's regions of one id: what `docs/RSS-COUNTS.md` §4 calls a view's
//! *cover*.
//!
//! A sorted list of runs `(first, end, count)`, non-overlapping, none of
//! count zero, and no two adjacent runs of the same count: the canonical
//! form, in which a cover has exactly one representation. An index in no run
//! is covered zero times.
//!
//! Every change is made in place, in room had first. [`Cover::room_for`] says
//! how many runs more a change, or any first part of it made in steps, may
//! need, and [`Cover::reserve`] has that room before anything changes, charged
//! to the running job as the region map's own room is. A change that would
//! need more refuses with [`CoverError::NoRoom`] and changes nothing, so a
//! change in room never allocates. Lowering what was raised before, exactly,
//! needs no room at all: it gives back the form the raise started from.

use alloc::vec::Vec;

use ferrix_kmem::{Charge, buffer_footprint};

/// The runs more a raise has room for so that a lower undoing it exactly is
/// always in room: the two edges it may have joined, and one for the step.
pub const UNDO_ROOM: usize = 3;

/// One run of indexes covered the same number of times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// The first index.
    pub first: u64,
    /// One past the last.
    pub end: u64,
    /// How many times each index of it is covered: never zero.
    pub count: u32,
}

/// Why a cover would not change. Nothing changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverError {
    /// The range is empty or ends before it starts.
    Empty,
    /// Not enough room had for the change: [`Cover::reserve`] first.
    NoRoom,
    /// A lower reaches an index covered no times, or a raise one covered
    /// `u32::MAX` times.
    Bounds,
    /// No memory, or the job is at its limit, for the room asked.
    NoMemory,
}

/// The cover itself. See the module.
#[derive(Debug)]
pub struct Cover {
    /// The runs, lowest first, canonical.
    pub(crate) runs: Vec<Run>,
    /// The room the list holds, charged.
    pub(crate) room: Charge,
}

impl Cover {
    /// Covering nothing, holding no room.
    ///
    /// # Errors
    ///
    /// [`CoverError::NoMemory`] if even an empty charge is refused.
    pub fn new() -> Result<Cover, CoverError> {
        Ok(Cover {
            runs: Vec::new(),
            room: Charge::bytes(0).map_err(|_| CoverError::NoMemory)?,
        })
    }

    /// Whether no index is covered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The runs, lowest first.
    #[must_use]
    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    /// How many times `index` is covered: a binary search.
    #[must_use]
    pub fn at(&self, index: u64) -> u32 {
        let after = self.runs.partition_point(|run| run.first <= index);
        after
            .checked_sub(1)
            .and_then(|at| self.runs.get(at))
            .filter(|run| index < run.end)
            .map_or(0, |run| run.count)
    }

    /// The runs a raise (`raise`) or a lower of `first..end` may add while it
    /// is made, whole or in any number of steps from `first` up: two edges
    /// split and one more edge split by the step under way, and for a raise
    /// every gap in the range filled. A lower that exactly undoes a raise may
    /// still split an edge the raise joined, so a caller that may have to undo
    /// one asks for [`UNDO_ROOM`] more with the raise.
    #[must_use]
    pub fn room_for(&self, first: u64, end: u64, raise: bool) -> usize {
        if !raise {
            return 3;
        }
        3 + self.gaps(first, end)
    }

    /// How many gaps -- runs of indexes covered no times -- `first..end`
    /// holds.
    fn gaps(&self, first: u64, end: u64) -> usize {
        let lo = self.runs.partition_point(|run| run.end <= first);
        let hi = self.runs.partition_point(|run| run.first < end);
        let mut gaps = 0;
        let mut at = first;
        for run in self.runs.get(lo..hi).unwrap_or(&[]) {
            if run.first > at {
                gaps += 1;
            }
            at = run.end;
        }
        if at < end {
            gaps += 1;
        }
        gaps
    }

    /// Whether `at` is inside a run, not at its edge: where a change from or
    /// to it splits that run.
    fn inside(&self, at: u64) -> bool {
        let index = self.runs.partition_point(|run| run.end <= at);
        self.runs
            .get(index)
            .is_some_and(|run| run.first < at && at < run.end)
    }

    /// Have room for `extra` runs more than the list holds, charged first.
    ///
    /// # Errors
    ///
    /// [`CoverError::NoMemory`], with nothing changed.
    pub fn reserve(&mut self, extra: usize) -> Result<(), CoverError> {
        let (len, room) = (self.runs.len(), self.runs.capacity());
        let needed = len.checked_add(extra).ok_or(CoverError::NoMemory)?;
        if needed <= room {
            return Ok(());
        }
        let target = needed.max(room.saturating_mul(2)).max(4);
        let (wanted, had) = (
            buffer_footprint::<Run>(target),
            buffer_footprint::<Run>(room),
        );
        // NOALLOC: a charge's resize moves an account, and refuses past the limit.
        self.room.resize(wanted).map_err(|_| CoverError::NoMemory)?;
        if ferrix_fallible::try_reserve(&mut self.runs, target - len).is_err() {
            // NOALLOC: a charge's resize moves an account.
            let _ = self.room.resize(had);
            return Err(CoverError::NoMemory);
        }
        let held = buffer_footprint::<Run>(self.runs.capacity());
        // NOALLOC: a charge's resize moves an account.
        let _ = self.room.resize(held);
        Ok(())
    }

    /// Cover `first..end` once more. Allocates nothing.
    ///
    /// # Errors
    ///
    /// As [`CoverError`]; nothing changed.
    pub fn raise(&mut self, first: u64, end: u64) -> Result<(), CoverError> {
        self.change(first, end, true)
    }

    /// Cover `first..end` once less: every index of it must be covered.
    /// Allocates nothing.
    ///
    /// # Errors
    ///
    /// As [`CoverError`]; nothing changed.
    pub fn lower(&mut self, first: u64, end: u64) -> Result<(), CoverError> {
        self.change(first, end, false)
    }

    /// [`Cover::raise`] or [`Cover::lower`].
    fn change(&mut self, first: u64, end: u64, raise: bool) -> Result<(), CoverError> {
        if first >= end {
            return Err(CoverError::Empty);
        }
        let lo = self.runs.partition_point(|run| run.end <= first);
        let hi = self.runs.partition_point(|run| run.first < end);
        let span = self.runs.get(lo..hi).unwrap_or(&[]);
        // Refusals first, so that nothing changes on one.
        if raise {
            if span.iter().any(|run| run.count == u32::MAX) {
                return Err(CoverError::Bounds);
            }
        } else {
            let mut at = first;
            for run in span {
                if run.first > at {
                    return Err(CoverError::Bounds);
                }
                at = run.end;
            }
            if at < end {
                return Err(CoverError::Bounds);
            }
        }
        // Exactly the runs this change inserts before it tidies.
        let extra = usize::from(self.inside(first))
            + usize::from(self.inside(end))
            + if raise { self.gaps(first, end) } else { 0 };
        if self.runs.len().saturating_add(extra) > self.runs.capacity() {
            return Err(CoverError::NoRoom);
        }
        // NOALLOC from here: room for every insert below was checked above.
        self.split_at(first);
        self.split_at(end);
        let lo = self.runs.partition_point(|run| run.end <= first);
        if raise {
            // Fill the gaps of `first..end` with runs of count zero.
            let mut at = first;
            let mut index = lo;
            while at < end {
                match self.runs.get(index) {
                    Some(run) if run.first == at => {
                        at = run.end;
                        index += 1;
                    }
                    next => {
                        let gap_end = next.map_or(end, |run| run.first.min(end));
                        // NOALLOC: room for every gap was checked above.
                        self.runs.insert(
                            index,
                            Run {
                                first: at,
                                end: gap_end,
                                count: 0,
                            },
                        );
                        at = gap_end;
                        index += 1;
                    }
                }
            }
        }
        let hi = self.runs.partition_point(|run| run.first < end);
        for run in self.runs.get_mut(lo..hi).into_iter().flatten() {
            run.count = if raise { run.count + 1 } else { run.count - 1 };
        }
        self.tidy(lo.saturating_sub(1), hi.saturating_add(1));
        Ok(())
    }

    /// Split the run holding `at` in two at it, if `at` is inside one.
    /// Called only with room for one more run.
    fn split_at(&mut self, at: u64) {
        let index = self.runs.partition_point(|run| run.end <= at);
        if let Some(run) = self.runs.get_mut(index)
            && run.first < at
            && at < run.end
        {
            let right = Run {
                first: at,
                end: run.end,
                count: run.count,
            };
            run.end = at;
            // NOALLOC: called only with room for one more run.
            self.runs.insert(index + 1, right);
        }
    }

    /// Make `from..to` of the list canonical again: runs of count zero out,
    /// adjacent runs of one count joined. Allocates nothing.
    fn tidy(&mut self, from: usize, to: usize) {
        let to = to.min(self.runs.len());
        let mut write = from;
        for read in from..to {
            let Some(&run) = self.runs.get(read) else {
                break;
            };
            if run.count == 0 {
                continue;
            }
            let joins = write > from
                && self
                    .runs
                    .get(write - 1)
                    .is_some_and(|last| last.end == run.first && last.count == run.count);
            if joins {
                if let Some(last) = self.runs.get_mut(write - 1) {
                    last.end = run.end;
                }
            } else {
                if let Some(slot) = self.runs.get_mut(write) {
                    *slot = run;
                }
                write += 1;
            }
        }
        // The rest of the list moves down over what was dropped.
        let len = self.runs.len();
        if write < to {
            self.runs.copy_within(to..len, write);
            self.runs.truncate(len - (to - write));
        }
        // The run before `from` may now join the first one kept.
        if from > 0 {
            let joins = match (self.runs.get(from - 1), self.runs.get(from)) {
                (Some(left), Some(right)) => left.end == right.first && left.count == right.count,
                _ => false,
            };
            if joins {
                let right_end = self.runs.get(from).map_or(0, |run| run.end);
                if let Some(left) = self.runs.get_mut(from - 1) {
                    left.end = right_end;
                }
                let _ = self.runs.remove(from);
            }
        }
    }
}
