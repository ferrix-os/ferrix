//! Floods held: the rate limiter, the folding of repeats, and the ingest
//! rules (`docs/LOGGING.md` §7).
//!
//! Every record a source sends passes here before it is stored:
//!
//! 1. **The rules** (`/etc/ferrix/lode.d/*.rules`, [`Rule`]), in order: the
//!    first whose filter matches decides -- `drop`, `sample 1/N`, `level P`
//!    (drop what is less urgent than P), `fold DURATION` (this source's
//!    repeats folded over a window of its own), `limit BURST/DURATION`.
//! 2. **Folding**: a message that repeats the source's last one, within the
//!    fold window (30 s unless a rule says), is counted, not stored; when
//!    another message comes or the window ends, one record says
//!    `message repeated N times`.
//! 3. **The limiter**: per source and per priority class, a fixed window
//!    (30 s) and a burst -- errors 10,000, warnings 2,000, chatter 1,000 --
//!    as journald's `journal-ratelimit.c`, but never scaled by free disk
//!    space. A flood of chatter never suppresses the same source's errors.
//!
//! Nothing is dropped silently: every rule, fold and window that held a
//! record back writes a record saying how many, from which source, and --
//! for kernel records -- the first and last kernel sequence numbers it held
//! back, so `lode verify --kernel` can tell a gap a record accounts for from
//! one nothing does. The kernel's own records (facility 0) are never held
//! back: the kernel decides what it says.
//!
//! A source is the record's `UNIT`, else its `SYSLOG_IDENTIFIER`, else its
//! `FERRIX_SUBSYSTEM`, else its `_TRANSPORT`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::filter::{self, Expr};
use crate::ingest::kernel_seq;
use crate::record::{Record, level};

/// `MESSAGE_ID` of a record saying the limiter held records back.
pub const MESSAGE_ID_SUPPRESSED: &str = "d1c3a5e7f9b24d6e8a0c2e4f6a8b0d1c";
/// `MESSAGE_ID` of a record saying a message repeated.
pub const MESSAGE_ID_REPEATED: &str = "7e5c3a1f9d7b4e2c8a6f4d2b0e8c6a4f";
/// `MESSAGE_ID` of a record saying a rule held records back.
pub const MESSAGE_ID_RULE: &str = "3f1d9b7c5a3e4f2d8b6a4c2e0f8d6b4a";

/// The window everything counts over, in microseconds: 30 s.
pub const WINDOW_US: u64 = 30_000_000;

/// How often a rule that held records back says so, in microseconds: 10 s.
pub const RULE_WINDOW_US: u64 = 10_000_000;

/// Bursts per window: errors (0-3), warnings (4-5), chatter (6-7).
pub const BURSTS: [u32; 3] = [10_000, 2_000, 1_000];

/// A priority's class.
fn class_of(priority: u8) -> usize {
    match priority {
        0..=3 => 0,
        4 | 5 => 1,
        _ => 2,
    }
}

/// A record's source, as the limiter counts it.
#[must_use]
pub fn source(record: &Record) -> Vec<u8> {
    [
        "UNIT",
        "SYSLOG_IDENTIFIER",
        "FERRIX_SUBSYSTEM",
        "_TRANSPORT",
    ]
    .iter()
    .find_map(|name| record.get(name))
    .map_or_else(|| Vec::from(&b"unknown"[..]), Vec::from)
}

/// Whether a record is the kernel's own, which nothing holds back.
fn is_kernel(record: &Record) -> bool {
    record.get("_TRANSPORT") == Some(b"kernel") || record.get("SYSLOG_IDENTIFIER") == Some(b"lode")
}

/// What a rule does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Store none.
    Drop,
    /// Store one in N.
    Sample(u32),
    /// Store none less urgent than this level.
    Level(u8),
    /// Fold repeats over this many microseconds.
    Fold(u64),
    /// This burst per this many microseconds, for every class.
    Limit(u32, u64),
}

/// One ingest rule.
#[derive(Debug, Clone)]
pub struct Rule {
    /// Its line, as written, for the records that say what it did.
    pub text: String,
    /// What it does.
    pub action: Action,
    /// Which records.
    pub filter: Expr,
}

/// Why a rules file did not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleError {
    /// The line, from 1.
    pub line: usize,
    /// Why.
    pub why: String,
}

/// Read a rules file: one rule a line, `#` comments, blank lines skipped.
///
/// ```text
/// drop          where id = "kio*" and message ~ thumbnail
/// sample 1/100  where driver = "intel-xelp" and priority = debug
/// level notice  where unit = "snmpd.service"
/// fold 60s      where id = "chrome"
/// limit 50/10s  where subsystem = "intelgpu"
/// ```
///
/// # Errors
///
/// The first line that is not a rule.
pub fn parse_rules(text: &str) -> Result<Vec<Rule>, RuleError> {
    let mut rules = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.split_once('#').map_or(raw, |(before, _)| before).trim();
        // An alert is a rule too, the alerts' own (`crate::alert`).
        if line.is_empty() || crate::alert::is_alert(raw.trim()) {
            continue;
        }
        let error = |why: &str| RuleError {
            line: index + 1,
            why: String::from(why),
        };
        let (head, expr) = line
            .split_once(" where ")
            .ok_or_else(|| error("a rule needs `where` and a filter"))?;
        let filter = filter::parse(expr).map_err(|parse| error(parse.why))?;
        let mut words = head.split_whitespace();
        let action = match (words.next(), words.next()) {
            (Some("drop"), None) => Action::Drop,
            (Some("sample"), Some(rate)) => Action::Sample(
                rate.strip_prefix("1/")
                    .and_then(|n| n.parse().ok())
                    .filter(|&n: &u32| n > 0)
                    .ok_or_else(|| error("sample takes 1/N"))?,
            ),
            (Some("level"), Some(name)) => {
                Action::Level(level(name).ok_or_else(|| error("level takes a priority"))?)
            }
            (Some("fold"), Some(span)) => {
                Action::Fold(duration_us(span).ok_or_else(|| error("fold takes a duration"))?)
            }
            (Some("limit"), Some(rate)) => {
                let (burst, span) = rate
                    .split_once('/')
                    .ok_or_else(|| error("limit takes BURST/DURATION"))?;
                Action::Limit(
                    burst
                        .parse()
                        .map_err(|_| error("limit's burst is a number"))?,
                    duration_us(span).ok_or_else(|| error("limit takes BURST/DURATION"))?,
                )
            }
            _ => {
                return Err(error(
                    "drop, sample 1/N, level P, fold DURATION or limit N/DURATION",
                ));
            }
        };
        if words.next().is_some() {
            return Err(error("more before `where` than the action takes"));
        }
        rules.push(Rule {
            text: String::from(line),
            action,
            filter,
        });
    }
    Ok(rules)
}

/// `30s`, `5min`, `2h`, `1d`, `500ms` in microseconds.
#[must_use]
pub fn duration_us(text: &str) -> Option<u64> {
    let digits = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let number: u64 = text.get(..digits)?.parse().ok()?;
    let scale: u64 = match text.get(digits..)? {
        "ms" => 1_000,
        "" | "s" | "sec" => 1_000_000,
        "m" | "min" => 60_000_000,
        "h" => 3_600_000_000,
        "d" => 86_400_000_000,
        _ => return None,
    };
    number.checked_mul(scale)
}

/// Kernel sequence numbers held back: how many, and the first and last.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Held {
    /// How many records.
    count: u64,
    /// The first and last kernel sequence numbers among them.
    seqs: Option<(u64, u64)>,
}

impl Held {
    /// Count `record`.
    fn add(&mut self, record: &Record) {
        self.count += 1;
        if let Some(seq) = kernel_seq(record) {
            self.seqs = Some(match self.seqs {
                Some((first, last)) => (first.min(seq), last.max(seq)),
                None => (seq, seq),
            });
        }
    }

    /// Put the count and the sequence range on `record`.
    fn stamp(&self, record: &mut Record, name: &str) {
        record.push(name, alloc::format!("{}", self.count).as_bytes());
        if let Some((first, last)) = self.seqs {
            record.push("HELD_FIRST_SEQNUM", alloc::format!("{first}").as_bytes());
            record.push("HELD_LAST_SEQNUM", alloc::format!("{last}").as_bytes());
        }
    }
}

/// One source's state.
#[derive(Debug, Default)]
struct Source {
    /// When its window began.
    start: u64,
    /// How long its window is: [`WINDOW_US`], or a `limit` rule's.
    span: u64,
    /// Records per class in this window.
    counts: [u32; 3],
    /// Records held back per class in this window.
    held: [Held; 3],
    /// Its last message, and the record it came in.
    last: Option<Record>,
    /// Repeats of it folded since.
    repeats: Held,
    /// The last of them, when their numbers differed from the first's.
    varied: Option<Vec<u8>>,
}

/// One rule's state.
#[derive(Debug, Default)]
struct RuleState {
    /// Records it has seen, for `sample`.
    seen: u64,
    /// Records it held back in this window.
    held: Held,
    /// When its window began.
    start: u64,
}

/// The limiter, the folding and the rules.
#[derive(Debug, Default)]
pub struct Limiter {
    /// The rules, in order.
    rules: Vec<Rule>,
    /// Their state.
    rule_state: Vec<RuleState>,
    /// Each source's state.
    sources: BTreeMap<Vec<u8>, Source>,
    /// Blank lines from the console, not stored.
    blank: Held,
    /// When the first of them came.
    blank_since: u64,
}

impl Limiter {
    /// A limiter with `rules`.
    #[must_use]
    pub fn new(rules: Vec<Rule>) -> Self {
        let rule_state = rules.iter().map(|_| RuleState::default()).collect();
        Limiter {
            rules,
            rule_state,
            sources: BTreeMap::new(),
            blank: Held::default(),
            blank_since: 0,
        }
    }

    /// Decide about `record`, at `now` (monotonic microseconds). What is to
    /// be stored -- records that say what was held back, then the record
    /// itself if it passes -- is pushed to `out`.
    pub fn admit(&mut self, record: Record, now: u64, out: &mut Vec<Record>) {
        if record
            .get("_TRANSPORT")
            .is_some_and(|transport| transport == b"kernel" || transport == b"console")
            && record.message().iter().all(u8::is_ascii_whitespace)
        {
            if self.blank.count == 0 {
                self.blank_since = now;
            }
            self.blank.add(&record);
            return;
        }
        if is_kernel(&record) {
            out.push(record);
            return;
        }
        let mut fold_window: Option<u64> = None;
        let mut limit: Option<(u32, u64)> = None;
        // Each kind of rule is read on its own, the first that matches
        // deciding: the first `fold` sets the fold window, the first
        // `limit` the limit, and the first rule that holds records
        // (`drop`, `sample`, `level`) whether this one is held. So a fold
        // rule and a limit rule both apply to the same line.
        let mut decided = false;
        for (rule, state) in self.rules.iter().zip(self.rule_state.iter_mut()) {
            if !rule.filter.matches(&record) {
                continue;
            }
            let hold = match rule.action {
                Action::Fold(span) => {
                    let _ = fold_window.get_or_insert(span);
                    continue;
                }
                Action::Limit(burst, span) => {
                    let _ = limit.get_or_insert((burst, span));
                    continue;
                }
                _ if decided => continue,
                Action::Drop => true,
                Action::Sample(n) => {
                    state.seen += 1;
                    (state.seen - 1) % u64::from(n) != 0
                }
                // A line with no level is not known to be below `most`:
                // it is kept.
                Action::Level(most) => {
                    record.priority > most && record.priority != crate::record::UNSPECIFIED
                }
            };
            decided = true;
            if hold {
                if state.held.count == 0 {
                    state.start = now;
                }
                state.held.add(&record);
                return;
            }
        }
        let key = source(&record);
        let source = self.sources.entry(key.clone()).or_default();
        source.span = limit.map_or(WINDOW_US, |(_, span)| span);
        if now.saturating_sub(source.start) >= source.span {
            close_window(&key, source, now, out);
        }
        if let Some(last) = &source.last
            && last.priority == record.priority
            && now.saturating_sub(last.monotonic_us) < fold_window.unwrap_or(WINDOW_US)
            && similar(last.message(), record.message())
            // A mark is a point in time someone asked for: never folded.
            && record.get("MARK").is_none()
        {
            if last.message() != record.message() {
                source.varied = Some(Vec::from(record.message()));
            }
            source.repeats.add(&record);
            return;
        }
        end_repeats(source, out);
        let class = class_of(record.priority);
        let burst = limit.map_or_else(
            || BURSTS.get(class).copied().unwrap_or(1000),
            |(burst, _)| burst,
        );
        let (Some(count), Some(held)) = (source.counts.get_mut(class), source.held.get_mut(class))
        else {
            return;
        };
        *count += 1;
        if *count > burst {
            held.add(&record);
            // A held line breaks the run: what comes next is not a repeat
            // of the last line stored, which may lie far behind it.
            source.last = None;
            return;
        }
        source.last = Some(record.clone());
        out.push(record);
    }

    /// Close every window that has ended by `now`, and every rule's: the
    /// records that say what they held back, pushed to `out`. The service
    /// calls this on every wakeup.
    pub fn tick(&mut self, now: u64, out: &mut Vec<Record>) {
        for (key, source) in &mut self.sources {
            if now.saturating_sub(source.start) >= source.span.max(1) {
                close_window(key, source, now, out);
            }
            if let Some(last) = &source.last
                && now.saturating_sub(last.monotonic_us) >= WINDOW_US
            {
                end_repeats(source, out);
                source.last = None;
            }
        }
        for (rule, state) in self.rules.iter().zip(&mut self.rule_state) {
            if state.held.count > 0 && now.saturating_sub(state.start) >= RULE_WINDOW_US {
                let mut record = said(
                    now,
                    5,
                    MESSAGE_ID_RULE,
                    &alloc::format!(
                        "rule `{}` held back {} record(s)",
                        rule.text,
                        state.held.count
                    ),
                );
                record.push("RULE", rule.text.as_bytes());
                state.held.stamp(&mut record, "N_DROPPED");
                out.push(record);
                state.held = Held::default();
            }
        }
        if self.blank.count > 0 && now.saturating_sub(self.blank_since) >= RULE_WINDOW_US {
            self.say_blank(now, out);
        }
        self.sources.retain(|_, source| {
            source.last.is_some() || source.counts.iter().any(|&count| count > 0)
        });
    }
}

impl Limiter {
    /// Say the blank lines held back.
    fn say_blank(&mut self, now: u64, out: &mut Vec<Record>) {
        let mut record = said(
            now,
            7,
            MESSAGE_ID_SUPPRESSED,
            &alloc::format!(
                "{} blank line(s) from the console not stored",
                self.blank.count
            ),
        );
        self.blank.stamp(&mut record, "N_BLANK");
        out.push(record);
        self.blank = Held::default();
    }

    /// Say now everything held back so far -- every source's window and run
    /// of repeats, every rule's count -- as `lode rotate` asks, so that a
    /// `lode verify` after it finds every gap said.
    pub fn flush(&mut self, now: u64, out: &mut Vec<Record>) {
        for (key, source) in &mut self.sources {
            close_window(key, source, now, out);
            end_repeats(source, out);
            source.last = None;
        }
        for state in &mut self.rule_state {
            state.start = 0;
        }
        if self.blank.count > 0 {
            self.say_blank(now, out);
        }
        self.tick(now.max(RULE_WINDOW_US), out);
    }
}

/// End `source`'s window: the record that says what it held back, if any.
fn close_window(key: &[u8], source: &mut Source, now: u64, out: &mut Vec<Record>) {
    let total: u64 = source.held.iter().map(|held| held.count).sum();
    if total > 0 {
        let name = String::from_utf8_lossy(key);
        let [errors, warnings, chatter] = source.held;
        let mut record = said(
            now,
            4,
            MESSAGE_ID_SUPPRESSED,
            &alloc::format!(
                "Suppressed {total} message(s) from {name} in the last window ({} errors, {} warnings, {} chatter)",
                errors.count,
                warnings.count,
                chatter.count
            ),
        );
        record.push("SUPPRESSED_SOURCE", key);
        let mut all = Held::default();
        for held in source.held {
            all.count += held.count;
            all.seqs = match (all.seqs, held.seqs) {
                (Some((a, b)), Some((c, d))) => Some((a.min(c), b.max(d))),
                (one, None) | (None, one) => one,
            };
        }
        all.stamp(&mut record, "N_SUPPRESSED");
        out.push(record);
    }
    source.start = now;
    source.counts = [0; 3];
    source.held = [Held::default(); 3];
}

/// End `source`'s run of repeats: the record that says how many.
fn end_repeats(source: &mut Source, out: &mut Vec<Record>) {
    if source.repeats.count == 0 {
        return;
    }
    let Some(last) = &source.last else {
        return;
    };
    let mut record = last.clone();
    record.fields.retain(|field| {
        !matches!(
            field.name.as_str(),
            "MESSAGE" | "_KERNEL_SEQNUM" | "_SOURCE_MONOTONIC_TIMESTAMP" | "MESSAGE_ID"
        )
    });
    let numbers = if source.varied.is_some() {
        ", numbers varying"
    } else {
        ""
    };
    record.push(
        "MESSAGE",
        alloc::format!(
            "message repeated {} time(s){numbers}: {}",
            source.repeats.count,
            String::from_utf8_lossy(last.message())
        )
        .as_bytes(),
    );
    record.push("MESSAGE_ID", MESSAGE_ID_REPEATED.as_bytes());
    if let Some(varied) = source.varied.take() {
        record.push("LAST_MESSAGE", &varied);
    }
    source.repeats.stamp(&mut record, "REPEATED");
    out.push(record);
    source.repeats = Held::default();
}

/// A record lode writes about what it held back.
fn said(now: u64, priority: u8, id: &str, message: &str) -> Record {
    let mut record = Record::new(priority, 0, now);
    record.push("MESSAGE", message.as_bytes());
    record.push("MESSAGE_ID", id.as_bytes());
    record.push("SYSLOG_IDENTIFIER", b"lode");
    record.push("_TRANSPORT", b"lode");
    record
}

/// Whether two messages are the same but for their numbers: every run of
/// digits read as one, so a line a program writes every frame with a
/// changing size or place folds as an identical one does.
#[must_use]
pub fn similar(a: &[u8], b: &[u8]) -> bool {
    let mut left = a.iter().peekable();
    let mut right = b.iter().peekable();
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                while left.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    let _ = left.next();
                }
                while right.peek().is_some_and(|byte| byte.is_ascii_digit()) {
                    let _ = right.next();
                }
            }
            (Some(x), Some(y)) if x == y => {}
            _ => return false,
        }
    }
}
