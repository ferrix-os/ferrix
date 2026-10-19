//! Writes of a number, parsed as Linux parses each.
//!
//! Every one goes through the kernel's `strstrip` and then `kstrtoint` with
//! base 0: surrounding whitespace dropped, an optional sign, then `0x` for
//! hexadecimal, a leading `0` for octal, or decimal; nothing may follow the
//! digits. What each file then does with the number differs, and each
//! function here says what, with the errno Linux answers.

use crate::Refusal;

/// `text` without the whitespace around it, as the kernel's `strstrip`
/// leaves it: `isspace` is space, tab, newline, vertical tab, form feed and
/// carriage return.
pub fn strip(text: &[u8]) -> &[u8] {
    let space = |byte: &u8| matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r');
    let start = text
        .iter()
        .position(|byte| !space(byte))
        .unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|byte| !space(byte))
        .map_or(start, |last| last + 1);
    text.get(start..end).unwrap_or(&[])
}

/// A number as `kstrtoint(text, 0, …)` reads it, `text` already stripped.
///
/// # Errors
///
/// [`Refusal::Invalid`] for anything but a whole number; [`Refusal::Range`]
/// for one outside `i32`.
pub fn kstrtoint(text: &[u8]) -> Result<i32, Refusal> {
    let (negative, digits) = match text.split_first() {
        Some((b'-', rest)) => (true, rest),
        Some((b'+', rest)) => (false, rest),
        _ => (false, text),
    };
    let (radix, digits) = match digits {
        [b'0', b'x' | b'X', rest @ ..] => (16, rest),
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, digits),
    };
    if digits.is_empty() {
        return Err(Refusal::Invalid);
    }
    let mut value: i64 = 0;
    for &byte in digits {
        let digit = char::from(byte).to_digit(radix).ok_or(Refusal::Invalid)?;
        value = value
            .checked_mul(i64::from(radix))
            .and_then(|value| value.checked_add(i64::from(digit)))
            .filter(|&value| value <= i64::from(i32::MAX) + 1)
            .ok_or(Refusal::Range)?;
    }
    let value = if negative { -value } else { value };
    i32::try_from(value).map_err(|_| Refusal::Range)
}

/// Who a write to `cgroup.procs` moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The writer itself: Linux reads pid 0 so.
    Writer,
    /// The process with this pid.
    Pid(u32),
}

/// Parse a write to `cgroup.procs`: one pid, 0 meaning the writer.
///
/// # Errors
///
/// [`Refusal::Invalid`] for anything else, a negative pid included, and for
/// a number out of range: `cgroup_procs_write_start` answers every failure
/// of `kstrtoint` with `EINVAL`.
pub fn parse_procs(text: &[u8]) -> Result<Target, Refusal> {
    let pid = kstrtoint(strip(text)).map_err(|_| Refusal::Invalid)?;
    match u32::try_from(pid) {
        Ok(0) => Ok(Target::Writer),
        Ok(pid) => Ok(Target::Pid(pid)),
        Err(_) => Err(Refusal::Invalid),
    }
}

/// Parse a write to `cgroup.kill`, which takes `1` and nothing else.
///
/// # Errors
///
/// What `kstrtoint` refuses, as it refuses it, and [`Refusal::Range`] for any
/// other number.
pub fn parse_kill(text: &[u8]) -> Result<(), Refusal> {
    match kstrtoint(strip(text))? {
        1 => Ok(()),
        _ => Err(Refusal::Range),
    }
}

/// A limit on a subtree's size: `cgroup.max.depth` and
/// `cgroup.max.descendants`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// No limit, written and printed as `max`.
    Max,
    /// At most this many.
    At(u32),
}

impl Limit {
    /// Whether `count` is within it.
    pub fn allows(self, count: u32) -> bool {
        match self {
            Limit::Max => true,
            Limit::At(most) => count <= most,
        }
    }
}

/// Parse a write to `cgroup.max.depth` or `cgroup.max.descendants`: `max`,
/// or a count. Linux stores `max` as `INT_MAX`, so writing `2147483647` is
/// the same as writing `max`, and prints back as `max`.
///
/// # Errors
///
/// What `kstrtoint` refuses, as it refuses it, and [`Refusal::Range`] for a
/// negative count.
pub fn parse_limit(text: &[u8]) -> Result<Limit, Refusal> {
    let text = strip(text);
    if text == b"max" {
        return Ok(Limit::Max);
    }
    let count = kstrtoint(text)?;
    match u32::try_from(count) {
        Ok(count) if count == i32::MAX as u32 => Ok(Limit::Max),
        Ok(count) => Ok(Limit::At(count)),
        Err(_) => Err(Refusal::Range),
    }
}

/// Parse a write to `cgroup.type`. Linux takes `threaded` and nothing else,
/// not even `domain`, which a cgroup is until made threaded; Ferrix has no
/// threaded cgroups.
///
/// # Errors
///
/// [`Refusal::NotSupported`] for `threaded`, [`Refusal::Invalid`] for
/// anything else.
pub fn parse_type(text: &[u8]) -> Result<(), Refusal> {
    if strip(text) == b"threaded" {
        Err(Refusal::NotSupported)
    } else {
        Err(Refusal::Invalid)
    }
}

/// Parse a write to `cgroup.freeze`: `0` or `1`.
///
/// # Errors
///
/// What `kstrtoint` refuses, as it refuses it, and [`Refusal::Range`] for any
/// other number.
pub fn parse_freeze(text: &[u8]) -> Result<bool, Refusal> {
    match kstrtoint(strip(text))? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(Refusal::Range),
    }
}

/// A number as `kstrtoull(text, 0, …)` reads it, `text` already stripped: no
/// sign but an optional `+`, then the base `kstrtoint` detects.
///
/// # Errors
///
/// [`Refusal::Invalid`] for anything but a whole number; [`Refusal::Range`]
/// for one past `u64`.
pub fn kstrtoull(text: &[u8]) -> Result<u64, Refusal> {
    let digits = match text.split_first() {
        Some((b'+', rest)) => rest,
        _ => text,
    };
    let (radix, digits) = radix_of(digits);
    if digits.is_empty() {
        return Err(Refusal::Invalid);
    }
    digits.iter().try_fold(0_u64, |value, &byte| {
        let digit = char::from(byte).to_digit(radix).ok_or(Refusal::Invalid)?;
        value
            .checked_mul(u64::from(radix))
            .and_then(|value| value.checked_add(u64::from(digit)))
            .ok_or(Refusal::Range)
    })
}

/// The base a number is written in, as the kernel's `_parse_integer_fixup_radix`
/// finds it, and the digits after its prefix.
fn radix_of(digits: &[u8]) -> (u32, &[u8]) {
    match digits {
        [b'0', b'x' | b'X', rest @ ..] if rest.first().is_some_and(u8::is_ascii_hexdigit) => {
            (16, rest)
        }
        [b'0', rest @ ..] if !rest.is_empty() => (8, rest),
        _ => (10, digits),
    }
}

/// `pids.max`'s largest count: Linux's `PIDS_MAX`, one past `PID_MAX_LIMIT`,
/// is what `max` means and what no count may reach.
pub const PIDS_MAX: u64 = 4 * 1024 * 1024 + 1;

/// Parse a write to `pids.max`, as `pids_max_write` does: `max`, or a count
/// read by `kstrtoll` below [`PIDS_MAX`]. `None` is `max`.
///
/// # Errors
///
/// What `kstrtoll` refuses, as it refuses it, and [`Refusal::Invalid`] for a
/// negative count or one at [`PIDS_MAX`] or past it.
pub fn parse_pids_max(text: &[u8]) -> Result<Option<u64>, Refusal> {
    let text = strip(text);
    if text == b"max" {
        return Ok(None);
    }
    let negative = text.first() == Some(&b'-');
    let magnitude = kstrtoull(if negative {
        text.get(1..).unwrap_or(&[])
    } else {
        text
    })?;
    if negative && magnitude != 0 {
        return Err(Refusal::Invalid);
    }
    if magnitude >= PIDS_MAX {
        return Err(Refusal::Invalid);
    }
    Ok(Some(magnitude))
}

/// Parse a write to `memory.max`, as `page_counter_memparse` does: `max`, or
/// a size in bytes as `memparse` reads it -- a number in the base its prefix
/// says, then at most one of the suffixes `K`, `M`, `G`, `T`, `P` or `E`, in
/// either case -- with nothing after. `None` is `max`. The kernel keeps it in
/// pages, rounded down; so does the caller.
///
/// # Errors
///
/// [`Refusal::Invalid`] for anything else. A size too large for `u64` is
/// taken as the largest, as the kernel's saturating page count takes it.
pub fn parse_memory_max(text: &[u8]) -> Result<Option<u64>, Refusal> {
    let text = strip(text);
    if text == b"max" {
        return Ok(None);
    }
    let (radix, digits) = radix_of(text);
    let count = digits
        .iter()
        .position(|&byte| char::from(byte).to_digit(radix).is_none())
        .unwrap_or(digits.len());
    // `simple_strtoull` reads the prefix's `0` as a number when no digit
    // follows it, and so does this.
    let (number, rest) = digits.split_at(count);
    if number.is_empty() && radix == 10 {
        return Err(Refusal::Invalid);
    }
    let value = number.iter().fold(0_u64, |value, &byte| {
        let digit = char::from(byte).to_digit(radix).unwrap_or(0);
        value
            .saturating_mul(u64::from(radix))
            .saturating_add(u64::from(digit))
    });
    let shift = match rest {
        [] => 0,
        [b'k' | b'K'] => 10,
        [b'm' | b'M'] => 20,
        [b'g' | b'G'] => 30,
        [b't' | b'T'] => 40,
        [b'p' | b'P'] => 50,
        [b'e' | b'E'] => 60,
        _ => return Err(Refusal::Invalid),
    };
    Ok(Some(
        value
            .checked_shl(shift)
            .filter(|shifted| shifted >> shift == value)
            .unwrap_or(u64::MAX),
    ))
}

/// `cpu.weight`'s least value, Linux's `CGROUP_WEIGHT_MIN`.
pub const WEIGHT_MIN: u64 = 1;
/// `cpu.weight`'s default, `CGROUP_WEIGHT_DFL`.
pub const WEIGHT_DEFAULT: u64 = 100;
/// `cpu.weight`'s greatest value, `CGROUP_WEIGHT_MAX`.
pub const WEIGHT_MAX: u64 = 10_000;

/// Parse a write to `cpu.weight`, as `cpu_weight_write_u64` takes it: a
/// number read by `kstrtoull`, from 1 to 10,000.
///
/// # Errors
///
/// What `kstrtoull` refuses, as it refuses it, and [`Refusal::Range`] for a
/// weight outside 1 to 10,000.
pub fn parse_weight(text: &[u8]) -> Result<u32, Refusal> {
    let weight = kstrtoull(strip(text))?;
    if !(WEIGHT_MIN..=WEIGHT_MAX).contains(&weight) {
        return Err(Refusal::Range);
    }
    u32::try_from(weight).map_err(|_| Refusal::Range)
}
