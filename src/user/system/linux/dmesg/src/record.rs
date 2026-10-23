//! A kernel log record, read from either format the kernel writes.

/// The facilities' names, in their numbers' order, as util-linux's `-x` and
/// `-f` spell them.
pub const FACILITIES: [&str; 24] = [
    "kern", "user", "mail", "daemon", "auth", "syslog", "lpr", "news", "uucp", "cron", "authpriv",
    "ftp", "res0", "res1", "res2", "res3", "local0", "local1", "local2", "local3", "local4",
    "local5", "local6", "local7",
];

/// The levels' names, in their numbers' order.
pub const LEVELS: [&str; 8] = [
    "emerg", "alert", "crit", "err", "warn", "notice", "info", "debug",
];

/// One record of the kernel log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its priority, the facility shifted left three and the level; `None`
    /// for a `syslog(2)` line that had no `<N>`.
    pub priority: Option<u32>,
    /// Its time since boot, in microseconds, where it has one.
    pub usec: Option<u64>,
    /// Its sequence number, from `/dev/kmsg`.
    pub seq: Option<u64>,
    /// Its text, unescaped, without its newline.
    pub text: Vec<u8>,
}

impl Record {
    /// Its level, 0 (`emerg`) to 7 (`debug`).
    #[must_use]
    pub fn level(&self) -> Option<u32> {
        self.priority.map(|priority| priority & 7)
    }

    /// Its facility: 0 for the kernel's.
    #[must_use]
    pub fn facility(&self) -> Option<u32> {
        self.priority.map(|priority| priority >> 3)
    }
}

/// A decimal number, all of `digits`.
fn number(digits: &[u8]) -> Option<u64> {
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    digits.iter().try_fold(0u64, |value, digit| {
        value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
    })
}

/// Read one `/dev/kmsg` record: `priority,seq,usec,flags[,more];text\n`,
/// then any ` KEY=value` lines, which are left out. The text's `\xNN`
/// escapes are undone.
#[must_use]
pub fn parse_kmsg(record: &[u8]) -> Option<Record> {
    let first = record.split(|&byte| byte == b'\n').next()?;
    let semicolon = first.iter().position(|&byte| byte == b';')?;
    let (header, text) = first.split_at(semicolon);
    let mut fields = header.split(|&byte| byte == b',');
    let priority = u32::try_from(number(fields.next()?)?).ok()?;
    let seq = number(fields.next()?)?;
    let usec = number(fields.next()?)?;
    Some(Record {
        priority: Some(priority),
        usec: Some(usec),
        seq: Some(seq),
        text: unescape(text.get(1..).unwrap_or_default()),
    })
}

/// Undo `/dev/kmsg`'s `\xNN` escapes; anything else is kept as it is.
#[must_use]
pub fn unescape(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(&byte) = text.get(at) {
        if byte == b'\\'
            && text.get(at + 1) == Some(&b'x')
            && let Some(value) = text
                .get(at + 2..at + 4)
                .and_then(|hex| core::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(value);
            at += 4;
        } else {
            out.push(byte);
            at += 1;
        }
    }
    out
}

/// Read what `syslog(2)`'s `READ_ALL` gave: lines of `<N>[seconds.micros]
/// text`, each part but the text optional, as util-linux reads them.
#[must_use]
pub fn parse_syslog(buffer: &[u8]) -> Vec<Record> {
    buffer
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(parse_syslog_line)
        .collect()
}

/// One line of [`parse_syslog`].
fn parse_syslog_line(line: &[u8]) -> Record {
    let mut rest = line;
    let mut priority = None;
    if let Some(after) = rest.strip_prefix(b"<")
        && let Some(close) = after.iter().position(|&byte| byte == b'>')
        && let Some(value) = after.get(..close).and_then(number)
    {
        priority = u32::try_from(value).ok();
        rest = after.get(close + 1..).unwrap_or_default();
    }
    let mut usec = None;
    if let Some(after) = rest.strip_prefix(b"[")
        && let Some(close) = after.iter().position(|&byte| byte == b']')
        && let Some(time) = after.get(..close).and_then(parse_time)
    {
        usec = Some(time);
        rest = after.get(close + 1..).unwrap_or_default();
        rest = rest.strip_prefix(b" ").unwrap_or(rest);
    }
    Record {
        priority,
        usec,
        seq: None,
        text: rest.to_vec(),
    }
}

/// `   12.345678` as microseconds.
fn parse_time(text: &[u8]) -> Option<u64> {
    let trimmed = text.trim_ascii();
    let dot = trimmed.iter().position(|&byte| byte == b'.')?;
    let seconds = number(trimmed.get(..dot)?)?;
    let fraction = trimmed.get(dot + 1..)?;
    if fraction.len() > 9 {
        return None;
    }
    let mut micros = number(fraction)?;
    for _ in fraction.len()..6 {
        micros = micros.checked_mul(10)?;
    }
    for _ in 6..fraction.len() {
        micros /= 10;
    }
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

/// What a reader of `/dev/kmsg` knows of the records the kernel dropped
/// before it read them: told by `EPIPE`, counted by the next record's number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Dropped {
    /// The number of the last record read.
    last: Option<u64>,
    /// Whether the kernel said records were dropped since.
    told: bool,
}

impl Dropped {
    /// The kernel answered `EPIPE`.
    pub fn lost(&mut self) {
        self.told = true;
    }

    /// `record` was read: the note to show before it, Linux's console words
    /// `** N printk messages dropped **` at `KERN_WARNING`, when records
    /// were dropped before it. Before the first record read, every number
    /// below it counts: a log cleared since boot makes that too many.
    pub fn before(&mut self, record: &Record) -> Option<Record> {
        let seq = record.seq?;
        let expected = self.last.map_or(0, |last| last.saturating_add(1));
        let told = core::mem::take(&mut self.told);
        self.last = Some(seq);
        let count = seq.saturating_sub(expected);
        (told && count > 0).then(|| Record {
            priority: Some(4),
            usec: record.usec,
            seq: None,
            text: format!("** {count} printk messages dropped **").into_bytes(),
        })
    }
}
