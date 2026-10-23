//! The protocols programs log by (`docs/LOGGING.md` §5, step 3): syslog on
//! `/dev/log`, systemd's native protocol on `/run/systemd/journal/socket`,
//! and systemd's stdout streams on `/run/systemd/journal/stdout`, read as
//! journald reads them.
//!
//! Every field a sender writes is untrusted: one beginning with `_` is
//! dropped ([`Record::push_untrusted`]); the trusted ones are the service's
//! to add from the kernel's word about the sender.

use alloc::string::String;
use alloc::vec::Vec;

use crate::record::{Record, valid_name};

/// A syslog datagram, RFC 5424 when it begins `<PRI>1 `, otherwise RFC 3164
/// read leniently, as journald's `syslog_parse_identifier` reads it:
/// `<PRI>`, an optional `Mmm dd hh:mm:ss ` timestamp, `ident[pid]: `, the
/// message. What is not there is not made up.
#[must_use]
pub fn syslog(datagram: &[u8], realtime_us: u64, monotonic_us: u64) -> Record {
    let datagram = datagram.strip_suffix(b"\n").unwrap_or(datagram);
    let (priority, rest) = pri(datagram);
    let level = u8::try_from(priority & 7).unwrap_or(6);
    let facility = priority >> 3;
    let mut record = Record::new(level, realtime_us, monotonic_us);
    if let Some(rest) = rest.strip_prefix(b"1 ") {
        rfc5424(rest, &mut record);
    } else {
        rfc3164(rest, &mut record);
    }
    record.push("SYSLOG_FACILITY", alloc::format!("{facility}").as_bytes());
    record.push("_TRANSPORT", b"syslog");
    record
}

/// `<PRI>` and what follows; `<13>` (user.notice) where there is none.
fn pri(bytes: &[u8]) -> (u32, &[u8]) {
    if let Some(rest) = bytes.strip_prefix(b"<")
        && let Some(end) = rest.iter().position(|&byte| byte == b'>')
        && end <= 3
        && let Some(number) = core::str::from_utf8(rest.get(..end).unwrap_or_default())
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        && number <= 191
    {
        return (number, rest.get(end + 1..).unwrap_or_default());
    }
    (13, bytes)
}

/// RFC 3164: an optional timestamp, then `ident[pid]: message`.
fn rfc3164(mut rest: &[u8], record: &mut Record) {
    // `Mmm dd hh:mm:ss ` -- 16 bytes, the day space-padded.
    if rest.len() >= 16
        && rest.get(3) == Some(&b' ')
        && rest.get(6) == Some(&b' ')
        && rest.get(9) == Some(&b':')
        && rest.get(12) == Some(&b':')
        && rest.get(15) == Some(&b' ')
    {
        record.push_untrusted("SYSLOG_TIMESTAMP", rest.get(..15).unwrap_or_default());
        rest = rest.get(16..).unwrap_or_default();
    }
    let (identifier, pid, message) = identifier(rest);
    if let Some(identifier) = identifier {
        record.push_untrusted("SYSLOG_IDENTIFIER", identifier);
    }
    if let Some(pid) = pid {
        record.push_untrusted("SYSLOG_PID", pid);
    }
    record.push_untrusted("MESSAGE", message);
}

/// `ident[pid]: message`, `ident: message`, or the message alone.
fn identifier(bytes: &[u8]) -> (Option<&[u8]>, Option<&[u8]>, &[u8]) {
    let end = bytes
        .iter()
        .position(|&byte| byte == b':' || byte == b'[' || byte == b' ')
        .unwrap_or(bytes.len());
    let name = bytes.get(..end).unwrap_or_default();
    if name.is_empty() {
        return (None, None, bytes);
    }
    let after = bytes.get(end..).unwrap_or_default();
    let (pid, after) = match after.strip_prefix(b"[") {
        Some(inside) => match inside.iter().position(|&byte| byte == b']') {
            Some(close) => (
                inside.get(..close),
                inside.get(close + 1..).unwrap_or_default(),
            ),
            None => return (None, None, bytes),
        },
        None => (None, after),
    };
    match after.strip_prefix(b":") {
        Some(message) => (
            Some(name),
            pid.filter(|pid| !pid.is_empty() && pid.iter().all(u8::is_ascii_digit)),
            message.strip_prefix(b" ").unwrap_or(message),
        ),
        None => (None, None, bytes),
    }
}

/// RFC 5424 after `<PRI>1 `: timestamp, host, app, procid, msgid,
/// structured data, message; `-` is nil.
fn rfc5424(rest: &[u8], record: &mut Record) {
    let mut fields = rest.splitn(6, |&byte| byte == b' ');
    let timestamp = fields.next().unwrap_or_default();
    let _host = fields.next();
    let app = fields.next().unwrap_or_default();
    let procid = fields.next().unwrap_or_default();
    let msgid = fields.next().unwrap_or_default();
    let tail = fields.next().unwrap_or_default();
    if timestamp != b"-" {
        record.push_untrusted("SYSLOG_TIMESTAMP", timestamp);
    }
    if app != b"-" && !app.is_empty() {
        record.push_untrusted("SYSLOG_IDENTIFIER", app);
    }
    if procid != b"-" && !procid.is_empty() {
        record.push_untrusted("SYSLOG_PID", procid);
    }
    if msgid != b"-" && !msgid.is_empty() {
        record.push_untrusted("SYSLOG_MSGID", msgid);
    }
    let (data, message) = structured(tail);
    if let Some(data) = data {
        record.push_untrusted("SYSLOG_STRUCTURED_DATA", data);
    }
    let message = message.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(message);
    record.push_untrusted("MESSAGE", message);
}

/// The structured data at the start of `tail` (`-`, or `[...]` elements,
/// a `]` inside a quoted value escaped `\]`), and the message after it.
fn structured(tail: &[u8]) -> (Option<&[u8]>, &[u8]) {
    if let Some(message) = tail.strip_prefix(b"-") {
        return (None, message.strip_prefix(b" ").unwrap_or(message));
    }
    if !tail.starts_with(b"[") {
        return (None, tail);
    }
    let mut at = 0;
    let mut quoted = false;
    while let Some(&byte) = tail.get(at) {
        match byte {
            b'\\' if quoted => at += 1,
            b'"' => quoted = !quoted,
            b']' if !quoted && tail.get(at + 1) != Some(&b'[') => {
                let data = tail.get(..=at).unwrap_or_default();
                let message = tail.get(at + 1..).unwrap_or_default();
                return (Some(data), message.strip_prefix(b" ").unwrap_or(message));
            }
            _ => {}
        }
        at += 1;
    }
    (Some(tail), &[])
}

/// One datagram of systemd's native protocol: `FIELD=value` lines, a value
/// with a newline in it written `FIELD\n<le64 length><bytes>\n`. `None` for
/// one that does not read, which journald drops too. `PRIORITY` sets the
/// level.
#[must_use]
pub fn native(datagram: &[u8], realtime_us: u64, monotonic_us: u64) -> Option<Record> {
    let mut record = Record::new(6, realtime_us, monotonic_us);
    let mut at = 0;
    while at < datagram.len() {
        let rest = datagram.get(at..)?;
        let line_end = rest
            .iter()
            .position(|&byte| byte == b'\n')
            .unwrap_or(rest.len());
        let line = rest.get(..line_end)?;
        if let Some(equals) = line.iter().position(|&byte| byte == b'=') {
            let name = core::str::from_utf8(line.get(..equals)?).ok()?;
            let value = line.get(equals + 1..)?;
            take(&mut record, name, value);
            at += line_end + 1;
        } else {
            let name = core::str::from_utf8(line).ok()?;
            let length_at = at + line_end + 1;
            let length =
                u64::from_le_bytes(datagram.get(length_at..length_at + 8)?.try_into().ok()?);
            let length = usize::try_from(length).ok()?;
            let value = datagram.get(length_at + 8..(length_at + 8).checked_add(length)?)?;
            take(&mut record, name, value);
            at = length_at + 8 + length;
            if datagram.get(at) == Some(&b'\n') {
                at += 1;
            }
        }
    }
    record.push("_TRANSPORT", b"journal");
    Some(record)
}

/// One field a sender gave: `PRIORITY` sets the level, a trusted name is
/// dropped, a name that is not one is dropped.
fn take(record: &mut Record, name: &str, value: &[u8]) {
    if name == "PRIORITY" {
        if let Some(level) = core::str::from_utf8(value)
            .ok()
            .and_then(|text| text.trim().parse::<u8>().ok())
            .filter(|&level| level <= 7)
        {
            record.priority = level;
        }
        return;
    }
    if valid_name(name) {
        record.push_untrusted(name, value);
    }
}

/// A stdout stream's header (journald's `stdout_stream_line`): seven lines
/// before the program's own output.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StreamHeader {
    /// `SYSLOG_IDENTIFIER`.
    pub identifier: String,
    /// The unit the sender says it is, kept as `UNIT` (untrusted).
    pub unit: String,
    /// The level a line is at unless it says otherwise.
    pub priority: u8,
    /// Whether a line's `<N>` prefix sets its level.
    pub level_prefix: bool,
}

/// Read a stream's header from the first seven lines of `bytes`: it, and
/// how many bytes it took; `None` while it is not whole, and for one that
/// does not read.
#[must_use]
pub fn stream_header(bytes: &[u8]) -> Option<(StreamHeader, usize)> {
    let mut lines = Vec::new();
    let mut at = 0;
    while lines.len() < 7 {
        let rest = bytes.get(at..)?;
        let end = rest.iter().position(|&byte| byte == b'\n')?;
        lines.push(core::str::from_utf8(rest.get(..end)?).ok()?);
        at += end + 1;
    }
    let number = |index: usize| {
        lines
            .get(index)
            .and_then(|text| text.trim().parse::<u8>().ok())
    };
    Some((
        StreamHeader {
            identifier: String::from(*lines.first()?),
            unit: String::from(*lines.get(1)?),
            priority: number(2).filter(|&level| level <= 7).unwrap_or(6),
            level_prefix: number(3) == Some(1),
        },
        at,
    ))
}

/// One line of a stream as a record.
#[must_use]
pub fn stream_line(
    header: &StreamHeader,
    line: &[u8],
    realtime_us: u64,
    monotonic_us: u64,
) -> Record {
    let (mut priority, mut text) = (header.priority, line);
    if header.level_prefix
        && let Some(rest) = line.strip_prefix(b"<")
        && let (Some(&digit), Some(&b'>')) = (rest.first(), rest.get(1))
        && digit.is_ascii_digit()
    {
        priority = (digit - b'0').min(7);
        text = rest.get(2..).unwrap_or_default();
    }
    let mut record = Record::new(priority, realtime_us, monotonic_us);
    record.push_untrusted("MESSAGE", text);
    if !header.identifier.is_empty() {
        record.push_untrusted("SYSLOG_IDENTIFIER", header.identifier.as_bytes());
    }
    if !header.unit.is_empty() {
        record.push_untrusted("UNIT", header.unit.as_bytes());
    }
    record.push("_TRANSPORT", b"stdout");
    record
}
