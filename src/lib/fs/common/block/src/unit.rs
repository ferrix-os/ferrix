//! A unit: one or more contiguous requests that will leave as one command.

use alloc::vec;
use alloc::vec::Vec;

use crate::limits::Limits;
use crate::request::{Op, Part};

/// Requests of one op covering one contiguous range, in submission order.
///
/// The range is the union of the parts' ranges, which [`Unit::join`] keeps
/// true by construction: it is only ever called on two units that
/// [`Unit::can_join`] said abut.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Unit {
    pub(crate) op: Op,
    pub(crate) sector: u64,
    pub(crate) count: u32,
    pub(crate) fua: bool,
    /// Set on a requeued part, which must reach the device on its own so that
    /// its own result says whether its own sectors are bad.
    pub(crate) isolated: bool,
    /// The earliest of the parts' deadlines.
    pub(crate) deadline: u64,
    /// In submission order.
    pub(crate) parts: Vec<Part>,
}

impl Unit {
    /// A unit of one request.
    pub(crate) fn single(op: Op, part: Part, isolated: bool) -> Self {
        Unit {
            op,
            sector: part.sector,
            count: part.count,
            fua: part.flags.fua,
            isolated,
            deadline: part.deadline,
            parts: vec![part],
        }
    }

    /// The first sector past the unit.
    ///
    /// Every range was checked against the capacity on submission, so this
    /// never saturates; saturating rather than failing keeps the index keys
    /// total without a second error path that cannot be taken.
    pub(crate) fn end(&self) -> u64 {
        self.sector.saturating_add(u64::from(self.count))
    }

    /// Whether `right` may be appended to `self`: the same op, neither a flush
    /// nor isolated, the same `fua`, `right` starting exactly where `self`
    /// ends, and the combination within the device's limits.
    pub(crate) fn can_join(&self, right: &Unit, limits: &Limits) -> bool {
        if self.op != right.op || matches!(self.op, Op::Flush) {
            return false;
        }
        if self.isolated || right.isolated || self.fua != right.fua {
            return false;
        }
        if self.end() != right.sector {
            return false;
        }
        let sectors_fit = self
            .count
            .checked_add(right.count)
            .is_some_and(|count| count <= limits.max_sectors());
        let parts_fit = self
            .parts
            .len()
            .checked_add(right.parts.len())
            .is_some_and(|parts| {
                u64::try_from(parts).unwrap_or(u64::MAX) <= u64::from(limits.max_parts())
            });
        sectors_fit && parts_fit
    }

    /// `self` followed by `right`, which [`Unit::can_join`] approved.
    pub(crate) fn join(mut self, right: Unit) -> Unit {
        self.count = self.count.saturating_add(right.count);
        self.deadline = self.deadline.min(right.deadline);
        self.parts.extend(right.parts);
        // A front merge puts the newer request's range first; the parts list
        // stays in submission order regardless.
        self.parts.sort_unstable_by_key(|part| part.seq);
        self
    }
}
