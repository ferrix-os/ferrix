//! The `io` controller's files: `io.max` and `io.stat`.
//!
//! Parsed and printed as Linux's `block/blk-throttle.c` (`tg_set_limit`,
//! `tg_prfill_limit`) and `block/blk-cgroup.c` (`blkg_conf_prep`,
//! `blkcg_print_stat`) do, against a 7.0 host's files. A line is a device,
//! `MAJ:MIN`, and its `key=value` words.

use alloc::vec::Vec;
use core::fmt::Write as _;

use crate::Refusal;

/// A `Vec<u8>` that `write!` can format into.
struct Out<'a>(&'a mut Vec<u8>);

impl core::fmt::Write for Out<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// `sscanf("%d")`'s digits: the leading decimal digits of `text`, what they
/// come to, and the rest.
fn number(text: &[u8]) -> Option<(u32, &[u8])> {
    let end = text.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if end == 0 {
        return None;
    }
    let (digits, rest) = text.split_at(end);
    let value = digits.iter().fold(0_u32, |value, &byte| {
        value
            .saturating_mul(10)
            .saturating_add(u32::from(byte - b'0'))
    });
    Some((value, rest))
}

/// The device a write to `io.max` names, and the words after it, as
/// `blkg_conf_prep` splits them: `MAJ:MIN` and white space.
///
/// # Errors
///
/// [`Refusal::Invalid`] when the line does not begin `MAJ:MIN `.
pub fn parse_device(text: &[u8]) -> Result<((u32, u32), &[u8]), Refusal> {
    let (major, rest) = number(text).ok_or(Refusal::Invalid)?;
    let rest = rest.strip_prefix(b":").ok_or(Refusal::Invalid)?;
    let (minor, rest) = number(rest).ok_or(Refusal::Invalid)?;
    let rest = match rest.first() {
        Some(byte) if byte.is_ascii_whitespace() => rest
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .and_then(|at| rest.get(at..))
            .unwrap_or(&[]),
        None => rest,
        Some(_) => return Err(Refusal::Invalid),
    };
    Ok(((major, minor), rest))
}

/// What a disk's `io.max` holds: bytes and operations a second, read and
/// write. `None` is `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// `rbps`.
    pub rbps: Option<u64>,
    /// `wbps`.
    pub wbps: Option<u64>,
    /// `riops`.
    pub riops: Option<u64>,
    /// `wiops`.
    pub wiops: Option<u64>,
}

impl Limits {
    /// No limit.
    pub const NONE: Limits = Limits {
        rbps: None,
        wbps: None,
        riops: None,
        wiops: None,
    };
}

/// The most operations a second a limit takes: `UINT_MAX`.
const IOPS_MAX: u64 = 0xffff_ffff;

/// Apply the words of a write to `io.max` to `limits`, as `tg_set_limit`
/// does: a word not given keeps its value.
///
/// # Errors
///
/// [`Refusal::Invalid`] for a word with no `=`, a value that is neither
/// digits nor `max`, an unknown key, or a byte limit of 1; [`Refusal::Range`]
/// for a value of 0.
pub fn parse_limits(text: &[u8], mut limits: Limits) -> Result<Limits, Refusal> {
    for word in text
        .split(u8::is_ascii_whitespace)
        .filter(|word| !word.is_empty())
    {
        let word = word.get(..26).unwrap_or(word);
        let equals = word
            .iter()
            .position(|&byte| byte == b'=')
            .ok_or(Refusal::Invalid)?;
        let (key, value) = word.split_at(equals);
        let value = value.get(1..).unwrap_or(&[]);
        let value = if value == b"max" {
            u64::MAX
        } else {
            let digits = value.iter().take_while(|byte| byte.is_ascii_digit());
            let mut any = false;
            let parsed = digits.fold(0_u64, |parsed, &byte| {
                any = true;
                parsed
                    .saturating_mul(10)
                    .saturating_add(u64::from(byte - b'0'))
            });
            if !any {
                return Err(Refusal::Invalid);
            }
            parsed
        };
        if value == 0 {
            return Err(Refusal::Range);
        }
        let limit = (value != u64::MAX).then_some(value);
        match key {
            b"rbps" if value > 1 => limits.rbps = limit,
            b"wbps" if value > 1 => limits.wbps = limit,
            b"riops" if value > 1 => limits.riops = limit.map(|rate| rate.min(IOPS_MAX)),
            b"wiops" if value > 1 => limits.wiops = limit.map(|rate| rate.min(IOPS_MAX)),
            _ => return Err(Refusal::Invalid),
        }
    }
    Ok(limits)
}

/// Append a disk's line of `io.max`: nothing if it has no limit.
pub fn render_max(out: &mut Vec<u8>, device: (u32, u32), limits: Limits) {
    if limits == Limits::NONE {
        return;
    }
    let show = |rate: Option<u64>| {
        rate.map_or(alloc::string::String::from("max"), |rate| {
            alloc::format!("{rate}")
        })
    };
    let _ = writeln!(
        Out(out),
        "{}:{} rbps={} wbps={} riops={} wiops={}",
        device.0,
        device.1,
        show(limits.rbps),
        show(limits.wbps),
        show(limits.riops),
        show(limits.wiops)
    );
}

/// What a disk's line of `io.stat` counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stat {
    /// Bytes read.
    pub rbytes: u64,
    /// Bytes written.
    pub wbytes: u64,
    /// Reads.
    pub rios: u64,
    /// Writes.
    pub wios: u64,
    /// Bytes discarded.
    pub dbytes: u64,
    /// Discards.
    pub dios: u64,
}

impl Stat {
    /// Whether nothing has been counted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Stat::default()
    }
}

/// Append a disk's line of `io.stat`: nothing if nothing was counted.
pub fn render_stat(out: &mut Vec<u8>, device: (u32, u32), stat: Stat) {
    if stat.is_empty() {
        return;
    }
    let _ = writeln!(
        Out(out),
        "{}:{} rbytes={} wbytes={} rios={} wios={} dbytes={} dios={}",
        device.0,
        device.1,
        stat.rbytes,
        stat.wbytes,
        stat.rios,
        stat.wios,
        stat.dbytes,
        stat.dios
    );
}
