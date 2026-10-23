//! Time: civil dates from microseconds since 1970, local time from a POSIX
//! `TZ` string, and the forms `--since` and `--until` take.
//!
//! A machine without a time-zone database still has local time: `TZ` is a
//! POSIX string (`CET-1CEST,M3.5.0,M10.5.0/3`), and a version 2 or later
//! `/etc/localtime` ends with one ([`tzif_footer`]), so neither needs
//! `/usr/share/zoneinfo`. The rules a POSIX string gives are this year's;
//! history before them is not kept, which only matters for a record older
//! than the last change of a zone's rules.

use alloc::string::String;

/// Microseconds in a second.
pub const SECOND: u64 = 1_000_000;

/// Days since 1970-01-01 to `(year, month, day)`, Howard Hinnant's
/// `civil_from_days`.
#[must_use]
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    (
        year,
        u32::try_from(m).unwrap_or(1),
        u32::try_from(d).unwrap_or(1),
    )
}

/// `(year, month, day)` to days since 1970-01-01, `days_from_civil`.
#[must_use]
pub fn days(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(month);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The day of the week of `days` since 1970, Sunday 0 (1970-01-01 was a
/// Thursday).
#[must_use]
pub fn weekday(days: i64) -> u32 {
    u32::try_from((days + 4).rem_euclid(7)).unwrap_or(0)
}

/// Broken-down time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    /// The year.
    pub year: i64,
    /// 1 to 12.
    pub month: u32,
    /// 1 to 31.
    pub day: u32,
    /// 0 to 23.
    pub hour: u32,
    /// 0 to 59.
    pub minute: u32,
    /// 0 to 59.
    pub second: u32,
    /// 0 to 999,999.
    pub micros: u32,
    /// The offset from UTC this was computed with, in seconds east.
    pub offset_s: i32,
}

impl Civil {
    /// `realtime_us` as the time `offset_s` east of UTC.
    #[must_use]
    pub fn at(realtime_us: u64, offset_s: i32) -> Self {
        let micros_total = i128::from(realtime_us) + i128::from(offset_s) * 1_000_000;
        let seconds = i64::try_from(micros_total.div_euclid(1_000_000)).unwrap_or(0);
        let micros = u32::try_from(micros_total.rem_euclid(1_000_000)).unwrap_or(0);
        let (year, month, day) = civil(seconds.div_euclid(86_400));
        let of_day = u32::try_from(seconds.rem_euclid(86_400)).unwrap_or(0);
        Civil {
            year,
            month,
            day,
            hour: of_day / 3600,
            minute: of_day / 60 % 60,
            second: of_day % 60,
            micros,
            offset_s,
        }
    }

    /// The offset as `+HH:MM`.
    #[must_use]
    pub fn offset(&self) -> String {
        let sign = if self.offset_s < 0 { '-' } else { '+' };
        let total = self.offset_s.unsigned_abs() / 60;
        alloc::format!("{sign}{:02}:{:02}", total / 60, total % 60)
    }
}

/// The month's three letters.
#[must_use]
pub fn month_name(month: u32) -> &'static str {
    const NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    usize::try_from(month.saturating_sub(1))
        .ok()
        .and_then(|index| NAMES.get(index))
        .copied()
        .unwrap_or("???")
}

/// A zone: UTC, or a POSIX `TZ` rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    /// Standard time's offset, seconds east of UTC.
    std_offset: i32,
    /// Daylight time's offset, and when it begins and ends, if the zone has it.
    dst: Option<Dst>,
}

/// Daylight saving time's part of a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Dst {
    /// Its offset, seconds east of UTC.
    offset: i32,
    /// When it begins, in local standard time.
    start: Rule,
    /// When it ends, in local daylight time.
    end: Rule,
}

/// `Mm.w.d/time`: the `w`th (5: last) weekday `d` of month `m`, at `time`
/// seconds after local midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rule {
    /// 1 to 12.
    month: u32,
    /// 1 to 5.
    week: u32,
    /// 0 (Sunday) to 6.
    weekday: u32,
    /// Seconds after midnight.
    time: i32,
}

impl Zone {
    /// UTC.
    #[must_use]
    pub const fn utc() -> Self {
        Zone {
            std_offset: 0,
            dst: None,
        }
    }

    /// A POSIX `TZ` string: `std offset [dst [offset] [,rule,rule]]`. A
    /// leading `:` or a name with no offset (`Europe/Berlin`) is not a rule,
    /// and is `None`. A daylight name with no rules uses the US rules, as
    /// glibc does.
    #[must_use]
    pub fn posix(text: &str) -> Option<Self> {
        let mut rest = text.trim();
        if rest.is_empty() || rest.starts_with(':') {
            return None;
        }
        rest = skip_name(rest)?;
        let (std_west, after) = offset(rest)?;
        rest = after;
        let std_offset = -std_west;
        if rest.is_empty() {
            return Some(Zone {
                std_offset,
                dst: None,
            });
        }
        rest = skip_name(rest)?;
        let mut dst_offset = std_offset + 3600;
        if !rest.is_empty()
            && !rest.starts_with(',')
            && let Some((west, after)) = offset(rest)
        {
            dst_offset = -west;
            rest = after;
        }
        let (start, end) = match rest.strip_prefix(',') {
            Some(rules) => {
                let (start, end) = rules.split_once(',')?;
                (rule(start)?, rule(end)?)
            }
            None => (rule("M3.2.0")?, rule("M11.1.0")?),
        };
        Some(Zone {
            std_offset,
            dst: Some(Dst {
                offset: dst_offset,
                start,
                end,
            }),
        })
    }

    /// The offset in force at `realtime_us`, seconds east of UTC.
    #[must_use]
    pub fn offset_at(&self, realtime_us: u64) -> i32 {
        let Some(dst) = self.dst else {
            return self.std_offset;
        };
        let seconds = i64::try_from(realtime_us / SECOND).unwrap_or(0);
        let (year, _, _) = civil((seconds + i64::from(self.std_offset)).div_euclid(86_400));
        let start = dst.start.utc_seconds(year, self.std_offset);
        let end = dst.end.utc_seconds(year, dst.offset);
        let inside = if start <= end {
            seconds >= start && seconds < end
        } else {
            seconds >= start || seconds < end
        };
        if inside { dst.offset } else { self.std_offset }
    }
}

impl Rule {
    /// When this rule falls in `year`, as seconds since 1970 UTC, given the
    /// offset in force just before it.
    fn utc_seconds(self, year: i64, offset: i32) -> i64 {
        let first = days(year, self.month, 1);
        let first_weekday = weekday(first);
        let mut day = 1 + (self.weekday + 7 - first_weekday) % 7 + (self.week - 1) * 7;
        let length = month_length(year, self.month);
        while day > length {
            day -= 7;
        }
        (first + i64::from(day) - 1) * 86_400 + i64::from(self.time) - i64::from(offset)
    }
}

/// Days in `month` of `year`.
fn month_length(year: i64, month: u32) -> u32 {
    let next = if month == 12 {
        days(year + 1, 1, 1)
    } else {
        days(year, month + 1, 1)
    };
    u32::try_from(next - days(year, month, 1)).unwrap_or(31)
}

/// Past a zone name: three or more letters, or `<...>`.
fn skip_name(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix('<') {
        return rest.split_once('>').map(|(_, after)| after);
    }
    let end = text
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    (end >= 3).then(|| text.get(end..).unwrap_or_default())
}

/// `[+-]hh[:mm[:ss]]` in seconds (west of UTC is positive, as POSIX has it),
/// and what follows.
fn offset(text: &str) -> Option<(i32, &str)> {
    let (sign, rest) = match text.as_bytes().first() {
        Some(b'-') => (-1, text.get(1..)?),
        Some(b'+') => (1, text.get(1..)?),
        _ => (1, text),
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(rest.len());
    let clock = rest.get(..end)?;
    if clock.is_empty() {
        return None;
    }
    Some((sign * clock_seconds(clock)?, rest.get(end..)?))
}

/// `hh[:mm[:ss]]` in seconds.
fn clock_seconds(text: &str) -> Option<i32> {
    let mut total = 0_i32;
    let mut scale = 3600;
    for part in text.split(':') {
        let value: i32 = part.parse().ok()?;
        total += value * scale;
        scale /= 60;
    }
    Some(total)
}

/// `Mm.w.d[/time]`. The `Jn` and `n` forms are not taken.
fn rule(text: &str) -> Option<Rule> {
    let (date, time) = match text.split_once('/') {
        Some((date, time)) => {
            let (sign, clock) = match time.strip_prefix('-') {
                Some(clock) => (-1, clock),
                None => (1, time),
            };
            (date, sign * clock_seconds(clock)?)
        }
        None => (text, 7200),
    };
    let mut parts = date.strip_prefix('M')?.split('.');
    let month: u32 = parts.next()?.parse().ok()?;
    let week: u32 = parts.next()?.parse().ok()?;
    let weekday: u32 = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=5).contains(&week) && weekday <= 6).then_some(Rule {
        month,
        week,
        weekday,
        time,
    })
}

/// The POSIX `TZ` string at the end of a version 2 or later `TZif` file
/// (`/etc/localtime`): the last line, between two newlines.
#[must_use]
pub fn tzif_footer(bytes: &[u8]) -> Option<&str> {
    if bytes.get(..4)? != b"TZif" || *bytes.get(4)? < b'2' {
        return None;
    }
    let body = bytes.strip_suffix(b"\n")?;
    let start = body.iter().rposition(|&byte| byte == b'\n')? + 1;
    core::str::from_utf8(body.get(start..)?).ok()
}

/// What `--since`, `--until` and a filter's times take, as microseconds
/// since 1970: `now`, `today`, `yesterday`, `tomorrow`; `-30s`, `+5min`,
/// `-2h`, `-1d`, `3 hours ago`; `@1700000000`; and `YYYY-MM-DD`,
/// `YYYY-MM-DD HH:MM[:SS[.ffffff]]` with a space or `T`, and `HH:MM[:SS]`
/// (today), each with an optional `Z`, `UTC` or `+HH:MM` -- every form lode
/// prints. A time with no zone is in `zone`.
#[must_use]
pub fn parse(text: &str, now_us: u64, zone: &Zone) -> Option<u64> {
    let text = text.trim();
    let now = i64::try_from(now_us).ok()?;
    let local_midnight = |offset_days: i64| -> Option<u64> {
        let offset = i64::from(zone.offset_at(now_us));
        let day = (now / 1_000_000 + offset).div_euclid(86_400) + offset_days;
        u64::try_from((day * 86_400 - offset) * 1_000_000).ok()
    };
    match text {
        "now" => return Some(now_us),
        "today" => return local_midnight(0),
        "yesterday" => return local_midnight(-1),
        "tomorrow" => return local_midnight(1),
        _ => {}
    }
    if let Some(seconds) = text.strip_prefix('@') {
        let seconds: u64 = seconds.parse().ok()?;
        return seconds.checked_mul(SECOND);
    }
    if let Some(delta) = relative(text) {
        return u64::try_from(now.checked_add(delta)?).ok();
    }
    absolute(text, now_us, zone)
}

/// `-30s`, `+5min`, `3 hours ago`: a signed number of microseconds.
fn relative(text: &str) -> Option<i64> {
    let (sign, rest) = if let Some(rest) = text.strip_suffix("ago") {
        (-1, rest.trim())
    } else if let Some(rest) = text.strip_prefix('-') {
        (-1, rest)
    } else {
        (1, text.strip_prefix('+')?)
    };
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let number: i64 = rest.get(..digits)?.parse().ok()?;
    let unit = rest.get(digits..)?.trim();
    let scale: i64 = match unit {
        "" | "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 604_800,
        _ => return None,
    };
    number
        .checked_mul(scale)?
        .checked_mul(1_000_000)?
        .checked_mul(sign)
}

/// A date, a date and time, or a time of today, with an optional zone.
fn absolute(text: &str, now_us: u64, zone: &Zone) -> Option<u64> {
    let (text, given_offset) = split_zone(text);
    let (date, clock) = match text.split_once(['T', ' ']) {
        Some((date, clock)) => (Some(date), Some(clock)),
        None if text.contains('-') => (Some(text), None),
        None => (None, Some(text)),
    };
    let (year, month, day) = match date {
        Some(date) => {
            let mut parts = date.split('-');
            let year: i64 = parts.next()?.parse().ok()?;
            let month: u32 = parts.next()?.parse().ok()?;
            let day: u32 = parts.next()?.parse().ok()?;
            if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return None;
            }
            (year, month, day)
        }
        None => {
            let today = Civil::at(now_us, zone.offset_at(now_us));
            (today.year, today.month, today.day)
        }
    };
    let micros_of_day = match clock {
        Some(clock) => clock_micros(clock)?,
        None => 0,
    };
    let local = days(year, month, day) * 86_400 * 1_000_000 + micros_of_day;
    let offset = match given_offset {
        Some(offset) => offset,
        None => {
            // The local time as if it were standard time, then the offset
            // in force there: right except in the hour a change repeats.
            let guess = local - i64::from(zone.std_offset) * 1_000_000;
            zone.offset_at(u64::try_from(guess).ok()?)
        }
    };
    u64::try_from(local - i64::from(offset) * 1_000_000).ok()
}

/// A time's zone suffix: `Z`, ` UTC`, or `+HH:MM`/`+HHMM`/`-HH:MM`.
fn split_zone(text: &str) -> (&str, Option<i32>) {
    if let Some(rest) = text.strip_suffix('Z') {
        return (rest, Some(0));
    }
    if let Some(rest) = text.strip_suffix(" UTC") {
        return (rest.trim_end(), Some(0));
    }
    let Some(at) = text.rfind(['+', '-']) else {
        return (text, None);
    };
    let (head, tail) = text.split_at(at);
    // A date's own dashes are not a zone: a zone follows a time.
    if !head.contains(':') {
        return (text, None);
    }
    let digits: String = tail.chars().skip(1).filter(char::is_ascii_digit).collect();
    let sign = if tail.starts_with('-') { -1 } else { 1 };
    let (hours, minutes) = match digits.len() {
        2 => (digits.parse::<i32>().ok(), Some(0)),
        4 => (
            digits.get(..2).and_then(|h| h.parse::<i32>().ok()),
            digits.get(2..).and_then(|m| m.parse::<i32>().ok()),
        ),
        _ => (None, None),
    };
    match (hours, minutes) {
        (Some(hours), Some(minutes)) => {
            (head.trim_end(), Some(sign * (hours * 3600 + minutes * 60)))
        }
        _ => (text, None),
    }
}

/// `HH:MM[:SS[.ffffff]]` in microseconds.
fn clock_micros(text: &str) -> Option<i64> {
    let mut parts = text.split(':');
    let hours: i64 = parts.next()?.parse().ok()?;
    let minutes: i64 = parts.next()?.parse().ok()?;
    let (seconds, micros) = match parts.next() {
        Some(seconds) => match seconds.split_once('.') {
            Some((whole, fraction)) => {
                let mut digits: String = fraction.chars().take(6).collect();
                while digits.len() < 6 {
                    digits.push('0');
                }
                (whole.parse::<i64>().ok()?, digits.parse::<i64>().ok()?)
            }
            None => (seconds.parse::<i64>().ok()?, 0),
        },
        None => (0, 0),
    };
    if parts.next().is_some() || hours > 23 || minutes > 59 || seconds > 60 {
        return None;
    }
    Some(((hours * 60 + minutes) * 60 + seconds) * 1_000_000 + micros)
}
