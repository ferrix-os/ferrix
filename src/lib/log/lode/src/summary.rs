//! A sealed segment's summary (`docs/LOGGING.md` §8.4, step 5): what a
//! reader needs to know about a segment without reading it.
//!
//! When the service seals a segment it writes `<segment>.sum` beside it:
//! the segment's sequence and time range, its record count, its priority
//! range, and every value it holds of the fields queries name most --
//! `SYSLOG_IDENTIFIER`, `UNIT`, `_SYSTEMD_UNIT`, `_TRANSPORT`,
//! `SESSION_ID`, `_FERRIX_DRIVER`, `_SYSTEMD_INVOCATION_ID` -- with a count
//! each. A reader skips a
//! segment whose summary says the query cannot match ([`Summary::may_match`]),
//! and lists boots and top talkers from summaries alone. A segment with no
//! summary (the one being written, one sealed by an older lode) is read.
//!
//! ```text
//! 0  magic "FXJSUM01"   8  u64 first seq   16  u64 last seq
//! 24 u64 first realtime  32 u64 last realtime  40 u64 records
//! 48 u8 lowest level    49 u8 highest level  50 u16 values
//! 52 values: u8 field, u16 length, bytes, u64 count
//! end u32 CRC-32C of everything before it
//! ```

use alloc::vec::Vec;

use crate::crc::crc32c;
use crate::filter::{Expr, Op, Selector};
use crate::record::Record;

/// The file's first bytes.
const MAGIC: &[u8; 8] = b"FXJSUM01";

/// The fields a summary keeps the values of, by their number in the file.
/// New fields go at the end: a reader that knows fewer ignores the values
/// of the ones it does not know.
pub const FIELDS: [&str; 7] = [
    "SYSLOG_IDENTIFIER",
    "UNIT",
    "_SYSTEMD_UNIT",
    "_TRANSPORT",
    "SESSION_ID",
    "_FERRIX_DRIVER",
    "_SYSTEMD_INVOCATION_ID",
];

/// At most this many values are kept; a segment with more is summarised
/// without them, and every query reads it.
const VALUES_MAX: usize = 4096;

/// A segment's summary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Summary {
    /// The first and last sequence numbers.
    pub seqs: (u64, u64),
    /// The first and last wall-clock times, microseconds.
    pub times: (u64, u64),
    /// How many records.
    pub records: u64,
    /// The most and least urgent levels (lowest number first).
    pub levels: (u8, u8),
    /// Each field's values and how many records hold each; `None` when
    /// there were too many to keep.
    pub values: Option<Vec<(u8, Vec<u8>, u64)>>,
}

impl Summary {
    /// The summary of `records`, a segment's, oldest first.
    #[must_use]
    pub fn of(records: &[Record]) -> Summary {
        let mut summary = Summary {
            levels: (7, 0),
            values: Some(Vec::new()),
            ..Summary::default()
        };
        let mut times = (u64::MAX, 0);
        for record in records {
            if summary.records == 0 {
                summary.seqs.0 = record.seqnum;
            }
            summary.seqs.1 = record.seqnum;
            times = (
                times.0.min(record.realtime_us),
                times.1.max(record.realtime_us),
            );
            summary.records += 1;
            summary.levels = (
                summary.levels.0.min(record.priority),
                summary.levels.1.max(record.priority),
            );
            for (index, name) in FIELDS.iter().enumerate() {
                let Some(value) = record.get(name) else {
                    continue;
                };
                let Some(values) = summary.values.as_mut() else {
                    break;
                };
                let field = u8::try_from(index).unwrap_or(0);
                if let Some(entry) = values
                    .iter_mut()
                    .find(|(f, v, _)| *f == field && v.as_slice() == value)
                {
                    entry.2 += 1;
                } else if values.len() < VALUES_MAX {
                    values.push((field, Vec::from(value), 1));
                } else {
                    summary.values = None;
                }
            }
        }
        summary.times = if summary.records == 0 { (0, 0) } else { times };
        summary
    }

    /// The file's bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::from(&MAGIC[..]);
        for value in [
            self.seqs.0,
            self.seqs.1,
            self.times.0,
            self.times.1,
            self.records,
        ] {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.push(self.levels.0);
        out.push(self.levels.1);
        let values = self.values.as_deref().unwrap_or_default();
        let kept = if self.values.is_some() {
            u16::try_from(values.len()).unwrap_or(u16::MAX)
        } else {
            u16::MAX
        };
        out.extend_from_slice(&kept.to_le_bytes());
        for (field, value, count) in values {
            out.push(*field);
            let value = value.get(..usize::from(u16::MAX)).unwrap_or(value);
            out.extend_from_slice(&u16::try_from(value.len()).unwrap_or(0).to_le_bytes());
            out.extend_from_slice(value);
            out.extend_from_slice(&count.to_le_bytes());
        }
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// Read a summary file; `None` for one that does not read whole.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Summary> {
        let body = bytes.get(..bytes.len().checked_sub(4)?)?;
        if crc32c(body) != u32::from_le_bytes(bytes.get(body.len()..)?.try_into().ok()?)
            || body.get(..8)? != MAGIC
        {
            return None;
        }
        let u64_at = |at: usize| -> Option<u64> {
            Some(u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?))
        };
        let kept = u16::from_le_bytes(body.get(50..52)?.try_into().ok()?);
        let mut summary = Summary {
            seqs: (u64_at(8)?, u64_at(16)?),
            times: (u64_at(24)?, u64_at(32)?),
            records: u64_at(40)?,
            levels: (*body.get(48)?, *body.get(49)?),
            values: None,
        };
        if kept != u16::MAX {
            let mut values = Vec::new();
            let mut at = 52;
            for _ in 0..kept {
                let field = *body.get(at)?;
                let length = usize::from(u16::from_le_bytes(
                    body.get(at + 1..at + 3)?.try_into().ok()?,
                ));
                let value = Vec::from(body.get(at + 3..at + 3 + length)?);
                let count = u64::from_le_bytes(
                    body.get(at + 3 + length..at + 11 + length)?
                        .try_into()
                        .ok()?,
                );
                values.push((field, value, count));
                at += 11 + length;
            }
            summary.values = Some(values);
        }
        Some(summary)
    }

    /// Whether a record of this segment could match `expr`: `false` only
    /// when the summary proves none can -- a conjunction with a priority
    /// range the segment's levels miss, or an equality on a summarised
    /// field whose glob no value of the segment matches.
    #[must_use]
    pub fn may_match(&self, expr: &Expr) -> bool {
        match expr {
            Expr::And(left, right) => self.may_match(left) && self.may_match(right),
            Expr::Or(left, right) => self.may_match(left) || self.may_match(right),
            Expr::Test(test)
                if test.op == Op::Eq || matches!(test.op, Op::Le | Op::Lt | Op::Ge | Op::Gt) =>
            {
                match &test.field {
                    Selector::Priority => {
                        let Some(wanted) = core::str::from_utf8(&test.value)
                            .ok()
                            .and_then(crate::record::level)
                        else {
                            return true;
                        };
                        let (low, high) = self.levels;
                        match test.op {
                            Op::Eq => (low..=high).contains(&wanted),
                            Op::Le => low <= wanted,
                            Op::Lt => low < wanted,
                            Op::Ge => high >= wanted,
                            _ => high > wanted,
                        }
                    }
                    Selector::Fields(names) if test.op == Op::Eq => {
                        let Some(values) = &self.values else {
                            return true;
                        };
                        let summarised: Vec<u8> = names
                            .iter()
                            .filter_map(|name| FIELDS.iter().position(|field| field == name))
                            .filter_map(|index| u8::try_from(index).ok())
                            .collect();
                        if summarised.len() != names.len() {
                            return true;
                        }
                        values.iter().any(|(field, value, _)| {
                            summarised.contains(field) && crate::filter::glob(&test.value, value)
                        })
                    }
                    Selector::Fields(_) => true,
                }
            }
            _ => true,
        }
    }

    /// Whether the segment holds anything between `since` and `until`.
    #[must_use]
    pub fn overlaps(&self, since: Option<u64>, until: Option<u64>) -> bool {
        since.is_none_or(|since| self.times.1 >= since)
            && until.is_none_or(|until| self.times.0 <= until)
    }
}
