//! Alerts: a rule with an action (`docs/LOGGING.md` §7.4, USER-WISHES
//! theme 22).
//!
//! An alert is a line of `/etc/ferrix/lode.d/*.rules`:
//!
//! ```text
//! alert [NAME] [rate > N/DURATION] [where EXPR] notify "TEXT"
//! alert [NAME] [rate > N/DURATION] [where EXPR] start UNIT
//! ```
//!
//! Without a rate, every record the filter lets through fires it; with
//! one, a source (the limiter's: unit, else identifier, else subsystem,
//! else transport) that writes more than N matching records within one
//! window of DURATION fires it. Each fires at most once per source in
//! [`DEDUPE_US`], so a flood is one alert, not a flood of them. Records
//! are counted as they arrive, before the limiter, so a flood the limiter
//! holds back is still seen.
//!
//! The action is never something lode does to the system itself
//! (§11.2): `notify` is a line to whoever listens on lode's
//! `/run/lode/alerts` -- the desktop's notifyd, a box run's `lode alerts` --
//! and `start` a request to init, which starts the unit only if its file
//! says `StartableBy=lode`. In `TEXT`, `%n` is the alert's name, `%s` the
//! source and `%c` how many records fired it; in `UNIT`, `%n` alone.

use alloc::string::String;
use alloc::vec::Vec;

use crate::filter::{self, Expr};
use crate::limit::{self, RuleError, duration_us};
use crate::record::Record;

/// How long an alert stays quiet for a source after firing for it.
pub const DEDUPE_US: u64 = 600_000_000;

/// `MESSAGE_ID` of the record that says an alert fired.
pub const MESSAGE_ID_ALERT: &str = "a1e47f1e0b5c4d3e8f2a6b9c0d1e2f3a";

/// The most sources an alert keeps a count for; past it, the oldest
/// window is forgotten.
const SOURCES_MAX: usize = 1024;

/// What an alert does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// A notification with this text.
    Notify(String),
    /// Ask init to start this unit.
    Start(String),
}

/// One alert.
#[derive(Debug, Clone)]
pub struct Alert {
    /// Its name: given, or `alert-<line>`.
    pub name: String,
    /// More than this many records within this window, in microseconds.
    pub rate: Option<(u64, u64)>,
    /// Which records count.
    pub filter: Option<Expr>,
    /// What it does.
    pub action: Action,
}

/// An alert that fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fired {
    /// The alert's name.
    pub name: String,
    /// The source it fired for.
    pub source: String,
    /// How many records fired it.
    pub count: u64,
    /// What to do, with `%` expanded.
    pub action: Action,
}

impl Fired {
    /// The record saying so, at `now_us` on both clocks given.
    #[must_use]
    pub fn record(&self, realtime_us: u64, monotonic_us: u64) -> Record {
        let (verb, what) = match &self.action {
            Action::Notify(text) => ("notify", text.as_str()),
            Action::Start(unit) => ("start", unit.as_str()),
        };
        let text = alloc::format!(
            "alert {} fired for {} ({} record(s)): {verb} {what}",
            self.name,
            self.source,
            self.count
        );
        let mut record = crate::ingest::own(
            4,
            MESSAGE_ID_ALERT,
            &text,
            crate::ingest::Now {
                realtime_us,
                monotonic_us,
            },
        );
        record.push("ALERT", self.name.as_bytes());
        record.push("ALERT_SOURCE", self.source.as_bytes());
        record.push("ALERT_ACTION", verb.as_bytes());
        record.push("N_RECORDS", alloc::format!("{}", self.count).as_bytes());
        record
    }

    /// The line `/run/lode/alerts` carries for a `notify`:
    /// `alert <name> <source> <text>`, the text on one line.
    #[must_use]
    pub fn line(&self) -> Option<String> {
        let Action::Notify(text) = &self.action else {
            return None;
        };
        let one = |text: &str| -> String {
            text.chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect()
        };
        Some(alloc::format!(
            "alert {} {} {}\n",
            one(&self.name).replace(' ', "_"),
            one(&self.source).replace(' ', "_"),
            one(text)
        ))
    }
}

/// Whether `line` (a comment taken off, trimmed) is an alert's.
#[must_use]
pub fn is_alert(line: &str) -> bool {
    line == "alert" || line.starts_with("alert ")
}

/// The alerts of a rules file; the other lines are the limiter's.
///
/// # Errors
///
/// The first alert line that does not read.
pub fn parse(text: &str) -> Result<Vec<Alert>, RuleError> {
    let mut alerts = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if !is_alert(line) {
            continue;
        }
        let error = |why: &str| RuleError {
            line: index + 1,
            why: String::from(why),
        };
        alerts.push(parse_one(line, index + 1).map_err(error)?);
    }
    Ok(alerts)
}

/// One `alert` line.
fn parse_one(line: &str, number: usize) -> Result<Alert, &'static str> {
    let rest = line.strip_prefix("alert").unwrap_or(line).trim_start();
    // The action is the last ` notify ` or ` start ` outside quotes.
    let (head, action) =
        split_action(rest).ok_or("an alert ends with notify \"TEXT\" or start UNIT")?;
    let action = match action {
        ("notify", text) => {
            let text = text.trim();
            let text = text
                .strip_prefix('"')
                .and_then(|text| text.strip_suffix('"'))
                .ok_or("notify takes a quoted text")?;
            Action::Notify(String::from(text))
        }
        (_, unit) => {
            let unit = unit.trim();
            if unit.is_empty() || unit.contains(char::is_whitespace) {
                return Err("start takes one unit");
            }
            Action::Start(String::from(unit))
        }
    };
    let (head, filter) = match head.split_once("where ") {
        Some((before, expr)) if before.is_empty() || before.ends_with(' ') => (
            before.trim(),
            Some(filter::parse(expr.trim()).map_err(|parse| parse.why)?),
        ),
        _ => (head.trim(), None),
    };
    let mut words: Vec<&str> = head.split_whitespace().collect();
    let mut rate = None;
    if let Some(at) = words.iter().position(|word| *word == "rate") {
        let (op, value) = (words.get(at + 1), words.get(at + 2));
        if op != Some(&">") {
            return Err("rate > N/DURATION");
        }
        let (count, span) = value
            .and_then(|value| value.split_once('/'))
            .ok_or("rate > N/DURATION")?;
        let count: u64 = count.parse().map_err(|_| "rate's N is a number")?;
        let span = if span.starts_with(|c: char| c.is_ascii_digit()) {
            String::from(span)
        } else {
            alloc::format!("1{span}")
        };
        let window = duration_us(&span)
            .filter(|&us| us > 0)
            .ok_or("rate's DURATION")?;
        rate = Some((count, window));
        let _ = words.drain(at..at + 3);
    }
    let name = match words.as_slice() {
        [] => alloc::format!("alert-{number}"),
        [name] => String::from(*name),
        _ => return Err("an alert's name is one word"),
    };
    Ok(Alert {
        name,
        rate,
        filter,
        action,
    })
}

/// `head` and (`notify`|`start`, what follows) at the last action word
/// outside quotes.
fn split_action(text: &str) -> Option<(&str, (&str, &str))> {
    let mut quoted = false;
    let mut found = None;
    for (at, c) in text.char_indices() {
        if c == '"' {
            quoted = !quoted;
            continue;
        }
        if quoted || !(at == 0 || text.get(..at).is_some_and(|before| before.ends_with(' '))) {
            continue;
        }
        let rest = text.get(at..)?;
        for verb in ["notify ", "start "] {
            if rest.starts_with(verb) {
                found = Some((at, verb.trim_end()));
            }
        }
    }
    let (at, verb) = found?;
    let head = text.get(..at)?;
    let what = text.get(at + verb.len()..)?;
    Some((head, (verb, what)))
}

/// The alerts and what each has counted.
#[derive(Debug, Default)]
pub struct Alerts {
    /// The alerts, in the order read.
    alerts: Vec<Alert>,
    /// Per alert, per source: its window's start and count, and when it
    /// last fired.
    state: Vec<Vec<Seen>>,
}

/// What an alert has seen of one source.
#[derive(Debug, Clone)]
struct Seen {
    /// The source.
    source: Vec<u8>,
    /// When its window began.
    since: u64,
    /// Matching records in it.
    count: u64,
    /// When it last fired, if it has.
    fired: Option<u64>,
}

impl Alerts {
    /// Alerts to watch for.
    #[must_use]
    pub fn new(alerts: Vec<Alert>) -> Alerts {
        let state = alerts.iter().map(|_| Vec::new()).collect();
        Alerts { alerts, state }
    }

    /// Whether there are any.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.alerts.is_empty()
    }

    /// Count `record`, arriving at `now_us` (monotonic), against every
    /// alert; the ones that fire go into `out`.
    pub fn observe(&mut self, record: &Record, now_us: u64, out: &mut Vec<Fired>) {
        // lode's own alert records never feed an alert.
        if record.get_str("MESSAGE_ID") == Some(MESSAGE_ID_ALERT) {
            return;
        }
        let source = limit::source(record);
        for (alert, seen) in self.alerts.iter().zip(self.state.iter_mut()) {
            if alert
                .filter
                .as_ref()
                .is_some_and(|expr| !expr.matches(record))
            {
                continue;
            }
            let at = match seen.iter().position(|each| each.source == source) {
                Some(at) => at,
                None => {
                    if seen.len() >= SOURCES_MAX {
                        let _ = seen.remove(0);
                    }
                    seen.push(Seen {
                        source: source.clone(),
                        since: now_us,
                        count: 0,
                        fired: None,
                    });
                    seen.len() - 1
                }
            };
            let Some(each) = seen.get_mut(at) else {
                continue;
            };
            let window = alert.rate.map_or(0, |(_, window)| window);
            if now_us.saturating_sub(each.since) >= window {
                each.since = now_us;
                each.count = 0;
            }
            each.count += 1;
            let over = alert.rate.is_none_or(|(limit, _)| each.count > limit);
            let quiet = each
                .fired
                .is_some_and(|fired| now_us.saturating_sub(fired) < DEDUPE_US);
            if over && !quiet {
                each.fired = Some(now_us);
                let shown = String::from_utf8_lossy(&source).into_owned();
                out.push(Fired {
                    name: alert.name.clone(),
                    source: shown.clone(),
                    count: each.count,
                    action: match &alert.action {
                        Action::Notify(text) => {
                            Action::Notify(expand(text, &alert.name, Some((&shown, each.count))))
                        }
                        Action::Start(unit) => Action::Start(expand(unit, &alert.name, None)),
                    },
                });
            }
        }
    }
}

/// `text` with `%n` the alert's name, and with `fired` (the source and the
/// count) `%s` and `%c`; `%%` is `%`, anything else after `%` as written.
fn expand(text: &str, name: &str, fired: Option<(&str, u64)>) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match (chars.next(), fired) {
            (Some('n'), _) => out.push_str(name),
            (Some('s'), Some((source, _))) => out.push_str(source),
            (Some('c'), Some((_, count))) => out.push_str(&alloc::format!("{count}")),
            (Some('%'), _) => out.push('%'),
            (Some(other), _) => {
                out.push('%');
                out.push(other);
            }
            (None, _) => out.push('%'),
        }
    }
    out
}
