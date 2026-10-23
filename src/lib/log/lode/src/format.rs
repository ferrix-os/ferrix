//! The output formats (`docs/LOGGING.md` §9.3).
//!
//! journald's are kept as journalctl prints them, since scripts read them:
//! `short`, `short-iso`, `short-precise`, `short-monotonic`, `with-unit`,
//! `cat`, `json` and `export`. lode's own default, `human`, shows the year
//! and the zone, the priority as a letter, and the message whole:
//!
//! ```text
//! 2026-10-23 14:01:02.123 +02:00 E hyprix[812]: message
//! ```

use alloc::string::String;
use alloc::vec::Vec;

use crate::cursor;
use crate::record::Record;
use crate::segment::push_hex;
use crate::time::{Civil, Zone, month_name};

/// An output format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// lode's default.
    Human,
    /// journald's `short`.
    Short,
    /// `short-iso`.
    ShortIso,
    /// `short-iso-precise`.
    ShortIsoPrecise,
    /// `short-precise`.
    ShortPrecise,
    /// `short-monotonic`.
    ShortMonotonic,
    /// `with-unit`.
    WithUnit,
    /// The message alone.
    Cat,
    /// One JSON object a line.
    Json,
    /// The Journal Export Format.
    Export,
}

impl Output {
    /// The format `-o NAME` names.
    #[must_use]
    pub fn named(name: &str) -> Option<Output> {
        Some(match name {
            "human" => Output::Human,
            "short" => Output::Short,
            "short-iso" => Output::ShortIso,
            "short-iso-precise" => Output::ShortIsoPrecise,
            "short-precise" => Output::ShortPrecise,
            "short-monotonic" => Output::ShortMonotonic,
            "with-unit" => Output::WithUnit,
            "cat" => Output::Cat,
            "json" => Output::Json,
            "export" => Output::Export,
            _ => return None,
        })
    }
}

/// The letter `human` shows for a level.
#[must_use]
pub fn level_letter(level: u8) -> char {
    match level {
        0 => 'X',
        1 => 'A',
        2 => 'C',
        3 => 'E',
        4 => 'W',
        5 => 'N',
        6 => 'I',
        _ => 'D',
    }
}

/// `record` in `output`, appended to `out`, with times in `zone`.
pub fn write(record: &Record, output: Output, zone: &Zone, out: &mut Vec<u8>) {
    let offset = zone.offset_at(record.realtime_us);
    let when = Civil::at(record.realtime_us, offset);
    match output {
        Output::Human => {
            push(
                out,
                &alloc::format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} {} {} ",
                    when.year,
                    when.month,
                    when.day,
                    when.hour,
                    when.minute,
                    when.second,
                    when.micros / 1000,
                    when.offset(),
                    level_letter(record.priority)
                ),
            );
            who(record, false, out);
            message(record, out);
        }
        Output::Short | Output::ShortPrecise | Output::WithUnit => {
            push(
                out,
                &alloc::format!(
                    "{} {:02} {:02}:{:02}:{:02}",
                    month_name(when.month),
                    when.day,
                    when.hour,
                    when.minute,
                    when.second
                ),
            );
            if output == Output::ShortPrecise {
                push(out, &alloc::format!(".{:06}", when.micros));
            }
            host_and_who(record, output == Output::WithUnit, out);
        }
        Output::ShortIso | Output::ShortIsoPrecise => {
            push(
                out,
                &alloc::format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    when.year,
                    when.month,
                    when.day,
                    when.hour,
                    when.minute,
                    when.second
                ),
            );
            if output == Output::ShortIsoPrecise {
                push(out, &alloc::format!(".{:06}", when.micros));
            }
            push(out, &when.offset());
            host_and_who(record, false, out);
        }
        Output::ShortMonotonic => {
            push(
                out,
                &alloc::format!(
                    "[{:5}.{:06}]",
                    record.monotonic_us / 1_000_000,
                    record.monotonic_us % 1_000_000
                ),
            );
            host_and_who(record, false, out);
        }
        Output::Cat => message(record, out),
        Output::Json => json(record, out),
        Output::Export => export(record, out),
    }
}

/// ` host who: message\n`, journald's tail of a short line.
fn host_and_who(record: &Record, unit: bool, out: &mut Vec<u8>) {
    out.push(b' ');
    push(
        out,
        if record.host.is_empty() {
            "localhost"
        } else {
            &record.host
        },
    );
    out.push(b' ');
    who(record, unit, out);
    message(record, out);
}

/// `ident[pid]: ` (or the unit's name, for `with-unit`).
fn who(record: &Record, unit: bool, out: &mut Vec<u8>) {
    let name = if unit {
        record
            .get("_SYSTEMD_UNIT")
            .or_else(|| record.get("UNIT"))
            .unwrap_or_else(|| record.identifier())
    } else {
        record.identifier()
    };
    printable(name, out);
    if let Some(pid) = record.pid() {
        out.push(b'[');
        printable(pid, out);
        out.push(b']');
    }
    out.extend_from_slice(b": ");
}

/// The message and a newline, control bytes shown `\xNN`, and a newline
/// inside it followed by an indent so the next line is seen to continue.
fn message(record: &Record, out: &mut Vec<u8>) {
    for &byte in record.message() {
        match byte {
            b'\n' => out.extend_from_slice(b"\n    "),
            b'\t' => out.push(byte),
            0..0x20 | 0x7f => push(out, &alloc::format!("\\x{byte:02x}")),
            _ => out.push(byte),
        }
    }
    out.push(b'\n');
}

/// `bytes` with control bytes shown `\xNN`.
fn printable(bytes: &[u8], out: &mut Vec<u8>) {
    for &byte in bytes {
        if byte < 0x20 || byte == 0x7f {
            push(out, &alloc::format!("\\x{byte:02x}"));
        } else {
            out.push(byte);
        }
    }
}

/// `text`'s bytes.
fn push(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
}

/// The store's own fields, as `json` and `export` begin with them.
fn own_fields(record: &Record) -> [(&'static str, String); 6] {
    let mut boot = String::new();
    push_hex(&mut boot, &record.boot_id);
    [
        ("__CURSOR", cursor::of(record)),
        (
            "__REALTIME_TIMESTAMP",
            alloc::format!("{}", record.realtime_us),
        ),
        (
            "__MONOTONIC_TIMESTAMP",
            alloc::format!("{}", record.monotonic_us),
        ),
        ("__SEQNUM", alloc::format!("{}", record.seqnum)),
        ("_BOOT_ID", boot),
        ("PRIORITY", alloc::format!("{}", record.priority)),
    ]
}

/// One JSON object, journald's: every value a string, a value that is not
/// UTF-8 an array of its bytes.
fn json(record: &Record, out: &mut Vec<u8>) {
    out.push(b'{');
    let mut first = true;
    let mut pair = |out: &mut Vec<u8>, name: &str, value: &[u8]| {
        if !first {
            out.push(b',');
        }
        first = false;
        json_string(name.as_bytes(), out);
        out.push(b':');
        if core::str::from_utf8(value).is_ok() {
            json_string(value, out);
        } else {
            out.push(b'[');
            for (index, byte) in value.iter().enumerate() {
                if index > 0 {
                    out.push(b',');
                }
                push(out, &alloc::format!("{byte}"));
            }
            out.push(b']');
        }
    };
    for (name, value) in own_fields(record) {
        pair(out, name, value.as_bytes());
    }
    if record.get("_HOSTNAME").is_none() && !record.host.is_empty() {
        pair(out, "_HOSTNAME", record.host.as_bytes());
    }
    for field in &record.fields {
        pair(out, &field.name, &field.value);
    }
    out.extend_from_slice(b"}\n");
}

/// `bytes`, which are UTF-8, as a JSON string.
fn json_string(bytes: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    for &byte in bytes {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0..0x20 | 0x7f => push(out, &alloc::format!("\\u{byte:04x}")),
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

/// The Journal Export Format: `NAME=value` lines, a value with a newline or
/// that is not UTF-8 written `NAME\n<le64 length><bytes>\n`, and a blank
/// line after the record.
fn export(record: &Record, out: &mut Vec<u8>) {
    let field = |out: &mut Vec<u8>, name: &str, value: &[u8]| {
        push(out, name);
        if value.contains(&b'\n') || core::str::from_utf8(value).is_err() {
            out.push(b'\n');
            out.extend_from_slice(&u64::try_from(value.len()).unwrap_or(0).to_le_bytes());
        } else {
            out.push(b'=');
        }
        out.extend_from_slice(value);
        out.push(b'\n');
    };
    for (name, value) in own_fields(record) {
        field(out, name, value.as_bytes());
    }
    if record.get("_HOSTNAME").is_none() && !record.host.is_empty() {
        field(out, "_HOSTNAME", record.host.as_bytes());
    }
    for each in &record.fields {
        field(out, &each.name, &each.value);
    }
    out.push(b'\n');
}
