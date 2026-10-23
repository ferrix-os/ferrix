//! A record as a line of output.

use crate::args::{Options, Time};
use crate::record::{FACILITIES, LEVELS, Record};

/// What `-H` remembers from one record to the next.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Human {
    /// The minute of the wall clock the last record shown fell in.
    minute: Option<i64>,
    /// That record's time since boot, in microseconds.
    last_usec: Option<u64>,
}

/// What a line needs from outside: when the machine booted.
pub trait Clock {
    /// When the machine booted: seconds and microseconds since the epoch.
    fn boot(&self) -> (i64, u32);
}

/// `/proc/uptime`'s first figure, `12345.67`, in microseconds.
#[must_use]
pub fn uptime_micros(text: &str) -> Option<u64> {
    let first = text.split_whitespace().next()?;
    let (seconds, fraction) = first.split_once('.').unwrap_or((first, ""));
    let seconds: u64 = seconds.parse().ok()?;
    let digits = fraction.get(..fraction.len().min(6))?;
    let mut micros: u64 = if digits.is_empty() {
        0
    } else {
        digits.parse().ok()?
    };
    for _ in digits.len()..6 {
        micros *= 10;
    }
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

/// The days' and months' names, as `strftime` gives them in the C locale.
const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// `seconds` since the epoch as a date and time in UTC, the only time zone
/// Ferrix has: (year, month 0-11, day 1-31, weekday 0-6 from Thursday, hour,
/// minute, second). Howard Hinnant's `civil_from_days`.
fn civil(seconds: i64) -> (i64, usize, i64, usize, i64, i64, i64) {
    let days = seconds.div_euclid(86_400);
    let of_day = seconds.rem_euclid(86_400);
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let of_era = shifted.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * of_year + 2) / 153;
    let day = of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 2
    } else {
        month_index - 10
    };
    let year = year_of_era + era * 400 + i64::from(month < 2);
    let weekday = usize::try_from(days.rem_euclid(7)).unwrap_or(0);
    (
        year,
        usize::try_from(month).unwrap_or(0),
        day,
        weekday,
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
    )
}

/// `%a %b %e %H:%M:%S %Y`, `-T`'s format: `Fri Oct 23 12:00:00 2026`.
fn ctime(seconds: i64) -> String {
    let (year, month, day, weekday, hour, minute, second) = civil(seconds);
    let day_name = DAYS.get(weekday).copied().unwrap_or("");
    let month_name = MONTHS.get(month).copied().unwrap_or("");
    format!("{day_name} {month_name} {day:>2} {hour:02}:{minute:02}:{second:02} {year}")
}

/// `%b%d %H:%M`, `-H`'s minute: `Oct23 12:00`.
fn minute_stamp(seconds: i64) -> String {
    let (_, month, day, _, hour, minute, _) = civil(seconds);
    let month_name = MONTHS.get(month).copied().unwrap_or("");
    format!("{month_name}{day:02} {hour:02}:{minute:02}")
}

/// Append `bytes` as text: printable bytes, tabs and UTF-8 as they are,
/// every other control byte as `\xNN`, as util-linux's `safe_fwrite` does.
fn text(bytes: &[u8], out: &mut Vec<u8>) {
    let mut rest = bytes;
    while !rest.is_empty() {
        match core::str::from_utf8(rest) {
            Ok(valid) => {
                escape_controls(valid.as_bytes(), out);
                return;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                escape_controls(valid, out);
                let bad = error.error_len().unwrap_or(after.len()).max(1);
                for byte in after.get(..bad).unwrap_or(after) {
                    out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
                }
                rest = after.get(bad..).unwrap_or_default();
            }
        }
    }
}

/// [`text`] over bytes known to be UTF-8.
fn escape_controls(valid: &[u8], out: &mut Vec<u8>) {
    for &byte in valid {
        if (byte < b' ' && byte != b'\t') || byte == 0x7f {
            out.extend_from_slice(format!("\\x{byte:02x}").as_bytes());
        } else {
            out.push(byte);
        }
    }
}

/// `[    1.234567]`: seconds in five columns, as util-linux and the kernel
/// print them.
fn monotonic(usec: u64) -> String {
    format!("[{:5}.{:06}]", usec / 1_000_000, usec % 1_000_000)
}

/// Append `record` to `out` as one line, as `options` say; `human` carries
/// `-H`'s state from the record before.
pub fn line(
    record: &Record,
    options: &Options,
    clock: &dyn Clock,
    human: &mut Human,
    out: &mut Vec<u8>,
) {
    if options.raw {
        if let Some(priority) = record.priority {
            out.extend_from_slice(format!("<{priority}>").as_bytes());
        }
        if let (Some(usec), true) = (record.usec, options.time != Time::None) {
            out.extend_from_slice(monotonic(usec).as_bytes());
            out.push(b' ');
        }
        text(&record.text, out);
        out.push(b'\n');
        return;
    }
    if options.decode
        && let (Some(facility), Some(level)) = (record.facility(), record.level())
    {
        let facility = usize::try_from(facility)
            .ok()
            .and_then(|at| FACILITIES.get(at))
            .copied()
            .unwrap_or("");
        let level = usize::try_from(level)
            .ok()
            .and_then(|at| LEVELS.get(at))
            .copied()
            .unwrap_or("");
        out.extend_from_slice(format!("{facility:<6}:{level:<6}: ").as_bytes());
    }
    if let Some(usec) = record.usec {
        let stamp = match options.time {
            Time::None => None,
            Time::Monotonic => Some(monotonic(usec)),
            Time::Ctime => Some(format!("[{}]", ctime(wall_seconds(clock, usec)))),
            Time::Human => Some(human_stamp(clock, usec, human)),
        };
        if let Some(stamp) = stamp {
            out.extend_from_slice(stamp.as_bytes());
            out.push(b' ');
        }
    }
    text(&record.text, out);
    out.push(b'\n');
}

/// The wall clock's seconds when a record `usec` after boot was made.
fn wall_seconds(clock: &dyn Clock, usec: u64) -> i64 {
    let (seconds, micros) = clock.boot();
    let since = i64::try_from(usec.saturating_add(u64::from(micros)) / 1_000_000).unwrap_or(0);
    seconds.saturating_add(since)
}

/// `-H`'s stamp: `[Oct23 14:01]` for the first record of a minute, and
/// `[  +0.000123]`, the time since the record before, for the rest.
fn human_stamp(clock: &dyn Clock, usec: u64, human: &mut Human) -> String {
    let wall = wall_seconds(clock, usec);
    let minute = wall.div_euclid(60);
    let stamp = match (human.minute, human.last_usec) {
        (Some(last), Some(before)) if last == minute => {
            let delta = usec.saturating_sub(before);
            format!("[{:>+5}.{:06}]", delta / 1_000_000, delta % 1_000_000)
        }
        _ => format!("[{}]", minute_stamp(wall)),
    };
    human.minute = Some(minute);
    human.last_usec = Some(usec);
    stamp
}
