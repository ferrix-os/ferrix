//! Which records are kept how long (`docs/LOGGING.md` §8.5).
//!
//! A retention class is a filter with an age and a size. A record goes to
//! the first class whose filter it matches, and each class is its own
//! series of segments (`<class>@<boot>-<seq>.fxj`), so expiry is removing
//! whole files, never rewriting one. `/etc/ferrix/lode.d/retention.conf`:
//!
//! ```text
//! class errors  where priority <= err          keep 1y   max 256M
//! class desktop where unit = "app-*"            keep 7d   max 512M
//! class debug   where priority = debug          ram-only
//! class system                                  keep 90d  max 1G
//! ```
//!
//! `ram-only` keeps a class in `/run/log/journal` alone, gone at power off
//! (fragile flash, systemd #14588). A class with no filter takes everything
//! left; without one, the last class does. Without the file, [`defaults`]:
//! errors a year, everything else 90 days, within the store's 1 GiB.

use alloc::string::String;
use alloc::vec::Vec;

use crate::filter::{self, Expr};
use crate::limit::{RuleError, duration_us};
use crate::record::Record;

/// A day, in microseconds: how long a segment is written before the next
/// is begun, so an age can expire it.
pub const DAY_US: u64 = 86_400_000_000;

/// One class.
#[derive(Debug, Clone)]
pub struct Class {
    /// Its name: lower-case letters, digits and `-`.
    pub name: String,
    /// Which records, or every one left.
    pub filter: Option<Expr>,
    /// How long its records are kept, if limited.
    pub keep_us: Option<u64>,
    /// How many bytes its segments may hold, if limited.
    pub max_bytes: Option<u64>,
    /// Kept in memory's store alone.
    pub ram_only: bool,
}

/// The classes without a file: errors a year, the rest 90 days.
#[must_use]
pub fn defaults() -> Vec<Class> {
    alloc::vec![
        Class {
            name: String::from("errors"),
            filter: filter::parse("priority <= err").ok(),
            keep_us: Some(365 * DAY_US),
            max_bytes: Some(256 << 20),
            ram_only: false,
        },
        Class {
            name: String::from("system"),
            filter: None,
            keep_us: Some(90 * DAY_US),
            max_bytes: Some(1 << 30),
            ram_only: false,
        },
    ]
}

/// Read a retention file.
///
/// # Errors
///
/// The first line that is not a class.
pub fn parse(text: &str) -> Result<Vec<Class>, RuleError> {
    let mut classes = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
        if line.is_empty() {
            continue;
        }
        let error = |why: &str| RuleError {
            line: index + 1,
            why: String::from(why),
        };
        let rest = line.strip_prefix("class ").ok_or_else(|| {
            error("a line is `class NAME [where FILTER] [keep AGE] [max SIZE] [ram-only]`")
        })?;
        let mut words: Vec<&str> = rest.split_whitespace().collect();
        let mut class = Class {
            name: String::new(),
            filter: None,
            keep_us: None,
            max_bytes: None,
            ram_only: false,
        };
        loop {
            match words.as_slice() {
                [.., "ram-only"] => {
                    class.ram_only = true;
                    let _ = words.pop();
                }
                [.., "keep", age] => {
                    class.keep_us =
                        Some(age_us(age).ok_or_else(|| error("keep takes an age: 7d, 1y"))?);
                    words.truncate(words.len() - 2);
                }
                [.., "max", size] => {
                    class.max_bytes =
                        Some(bytes(size).ok_or_else(|| error("max takes a size: 512M, 1G"))?);
                    words.truncate(words.len() - 2);
                }
                _ => break,
            }
        }
        let (name, filter) = match words.split_first() {
            Some((name, [])) => (*name, None),
            Some((name, [where_, expr @ ..])) if *where_ == "where" => {
                let text = expr.join(" ");
                (
                    *name,
                    Some(filter::parse(&text).map_err(|parse| error(parse.why))?),
                )
            }
            _ => return Err(error("a class needs a name, then `where` and a filter")),
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(error(
                "a class's name is lower-case letters, digits and `-`",
            ));
        }
        class.name = String::from(name);
        class.filter = filter;
        classes.push(class);
    }
    if classes.is_empty() {
        return Ok(defaults());
    }
    Ok(classes)
}

/// `7d`, `12h`, `2w`, `1y`, in microseconds.
fn age_us(text: &str) -> Option<u64> {
    if let Some(weeks) = text.strip_suffix('w') {
        return weeks.parse::<u64>().ok()?.checked_mul(7 * DAY_US);
    }
    if let Some(years) = text.strip_suffix('y') {
        return years.parse::<u64>().ok()?.checked_mul(365 * DAY_US);
    }
    duration_us(text)
}

/// `512K`, `64M`, `1G`, or a number of bytes.
fn bytes(text: &str) -> Option<u64> {
    let (number, shift) = match text.as_bytes().last()? {
        b'K' | b'k' => (text.get(..text.len() - 1)?, 10),
        b'M' | b'm' => (text.get(..text.len() - 1)?, 20),
        b'G' | b'g' => (text.get(..text.len() - 1)?, 30),
        _ => (text, 0),
    };
    number.parse::<u64>().ok()?.checked_mul(1 << shift)
}

/// The class `record` goes to: the first that matches, else the last.
#[must_use]
pub fn assign(classes: &[Class], record: &Record) -> usize {
    classes
        .iter()
        .position(|class| {
            class
                .filter
                .as_ref()
                .is_none_or(|filter| filter.matches(record))
        })
        .unwrap_or(classes.len().saturating_sub(1))
}

/// A segment, as vacuum sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// Its class's name.
    pub class: String,
    /// Its file's name.
    pub name: String,
    /// When it was begun.
    pub created_us: u64,
    /// Its size.
    pub size: u64,
    /// Whether it is being written: never removed.
    pub active: bool,
}

/// The segments to remove at `now`: each class's past its age (a segment
/// begun more than its age and a day ago holds nothing younger than the
/// age) and past its size, oldest first, then the store's oldest past
/// `store_max`. Segments of a class no longer configured are kept to the
/// store's limit alone.
#[must_use]
pub fn vacuum(classes: &[Class], segments: &[Kept], now_us: u64, store_max: u64) -> Vec<String> {
    let mut ordered: Vec<&Kept> = segments.iter().collect();
    ordered.sort_by_key(|kept| kept.created_us);
    let mut removed: Vec<String> = Vec::new();
    for class in classes {
        let mine: Vec<&&Kept> = ordered
            .iter()
            .filter(|kept| kept.class == class.name)
            .collect();
        let mut total: u64 = mine.iter().map(|kept| kept.size).sum();
        for kept in &mine {
            if kept.active {
                continue;
            }
            let too_old = class.keep_us.is_some_and(|keep| {
                kept.created_us.saturating_add(keep).saturating_add(DAY_US) < now_us
            });
            let too_big = class.max_bytes.is_some_and(|max| total > max);
            if too_old || too_big {
                total = total.saturating_sub(kept.size);
                removed.push(kept.name.clone());
            }
        }
    }
    let mut total: u64 = ordered
        .iter()
        .filter(|kept| !removed.contains(&kept.name))
        .map(|kept| kept.size)
        .sum();
    for kept in &ordered {
        if total <= store_max {
            break;
        }
        if kept.active || removed.contains(&kept.name) {
            continue;
        }
        total = total.saturating_sub(kept.size);
        removed.push(kept.name.clone());
    }
    removed
}

/// A size as journalctl takes one: bytes, or a number with `K`, `M`, `G`,
/// `T`, `P` or `E`, each 1024 of the one before; a fraction is allowed.
#[must_use]
pub fn size_bytes(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let shift = match unit.trim().trim_end_matches(['B', 'b']) {
        "" => 0,
        "K" | "k" => 10,
        "M" | "m" => 20,
        "G" | "g" => 30,
        "T" | "t" => 40,
        "P" | "p" => 50,
        "E" | "e" => 60,
        _ => return None,
    };
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    let whole: u64 = whole.parse().ok()?;
    let mut bytes = whole.checked_shl(shift)?;
    if !fraction.is_empty() {
        let digits: u64 = fraction.parse().ok()?;
        let scale = 10_u64.checked_pow(u32::try_from(fraction.len()).ok()?)?;
        bytes = bytes.checked_add(digits.checked_shl(shift)? / scale)?;
    }
    Some(bytes)
}

/// A span as systemd's `parse_sec` takes one, in seconds: numbers each
/// with a unit (`2weeks`, `1d 12h`, `90min`); a bare number is seconds.
#[must_use]
pub fn span_seconds(text: &str) -> Option<u64> {
    let mut rest = text.trim();
    let mut total: u64 = 0;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits == 0 {
            return None;
        }
        let number: u64 = rest.get(..digits)?.parse().ok()?;
        rest = rest.get(digits..)?.trim_start();
        let letters = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let unit = rest.get(..letters)?;
        rest = rest.get(letters..)?.trim_start();
        let seconds: u64 = match unit {
            "" | "s" | "sec" | "second" | "seconds" => 1,
            "m" | "min" | "minute" | "minutes" => 60,
            "h" | "hr" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            "w" | "week" | "weeks" => 604_800,
            "M" | "month" | "months" => 2_629_800,
            "y" | "year" | "years" => 31_557_600,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(seconds)?)?;
    }
    Some(total)
}
