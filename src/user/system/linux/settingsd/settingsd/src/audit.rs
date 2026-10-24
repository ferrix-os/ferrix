//! The audit record (`docs/SETTINGS-WRITER.md` §8): one line per request
//! that reached authorisation, on standard output, which init attributes
//! to `settings.service` and hands to lode (`docs/LOGGING.md` §5), and keeps
//! in its own ring for `svc log settings.service` when lode is not running.
//!
//! A line never holds a secret. Its values are fixed words, names the
//! requests' rules or `/etc/passwd` already limit, and a process name that
//! is escaped here, so no value can split a line in two or hide a field.

use std::io::Write as _;

/// Every line said, in a test build: what the tests read the audit from.
#[cfg(test)]
pub(crate) static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// Say `line` on standard output, whole.
pub(crate) fn say(line: &str) {
    #[cfg(test)]
    if let Ok(mut said) = SAID.lock() {
        said.push(line.to_owned());
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// `value` as one field: printable ASCII other than space, `=` and `\`;
/// anything else as `\xNN`. An empty value is `-`.
pub(crate) fn field(value: &str) -> String {
    if value.is_empty() {
        return "-".to_owned();
    }
    let mut text = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_graphic() && byte != b'=' && byte != b'\\' {
            text.push(char::from(byte));
        } else {
            text.push_str(&format!("\\x{byte:02x}"));
        }
    }
    text
}

/// One record: `settings: ` and the fields, each `key=value`.
pub(crate) fn record(fields: &[(&str, &str)]) -> String {
    let mut line = String::from("settings:");
    for (key, value) in fields {
        line.push(' ');
        line.push_str(key);
        line.push('=');
        line.push_str(&field(value));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_cannot_split_a_line_or_forge_a_field() {
        assert_eq!(
            record(&[("comm", "a b\nresult=done"), ("x", "")]),
            "settings: comm=a\\x20b\\x0aresult\\x3ddone x=-"
        );
    }
}
