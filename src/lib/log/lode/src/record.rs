//! A record: journald's `FIELD=value` pairs, with the store's stamps beside
//! them (`docs/LOGGING.md` §4).
//!
//! A field's name is 1 to 64 bytes of `A-Z`, `0-9` and `_`, not beginning
//! with a digit, as journald's are; its value is any bytes. Names beginning
//! with `_` are trusted: lode writes them from what the kernel or init says,
//! and drops any a sender wrote ([`Record::push_untrusted`]).

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

/// The longest field name journald takes.
pub const NAME_MAX: usize = 64;

/// The longest value lode keeps of a field; the rest is cut.
pub const VALUE_MAX: usize = 64 * 1024;

/// The longest `MESSAGE` lode keeps; the rest is cut and `_TRUNCATED=1` added.
pub const MESSAGE_MAX: usize = 256 * 1024;

/// One field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// Its name.
    pub name: String,
    /// Its value.
    pub value: Vec<u8>,
}

/// One record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The store's own sequence number, unique across boots on one machine.
    pub seqnum: u64,
    /// When it happened, in microseconds since 1970 (UTC).
    pub realtime_us: u64,
    /// When it happened, in microseconds since this boot began.
    pub monotonic_us: u64,
    /// The UTC offset in force when it was stored, in seconds.
    pub utc_offset_s: i32,
    /// Its level, 0 (emerg) to 7 (debug).
    pub priority: u8,
    /// The boot it belongs to: its segment's, not an entry's own.
    pub boot_id: [u8; 16],
    /// The machine's name, its segment's.
    pub host: Rc<str>,
    /// Its fields, in the order they were given.
    pub fields: Vec<Field>,
}

impl Record {
    /// A record with no fields yet.
    #[must_use]
    pub fn new(priority: u8, realtime_us: u64, monotonic_us: u64) -> Self {
        Record {
            seqnum: 0,
            realtime_us,
            monotonic_us,
            utc_offset_s: 0,
            priority: priority.min(7),
            boot_id: [0; 16],
            host: Rc::from(""),
            fields: Vec::new(),
        }
    }

    /// Add a field lode itself vouches for, the trusted `_` ones included.
    /// A name that is not a field name is dropped.
    pub fn push(&mut self, name: &str, value: &[u8]) {
        if !valid_name(name) {
            return;
        }
        let limit = if name == "MESSAGE" {
            MESSAGE_MAX
        } else {
            VALUE_MAX
        };
        let cut = value.len() > limit;
        self.fields.push(Field {
            name: String::from(name),
            value: Vec::from(value.get(..limit).unwrap_or(value)),
        });
        if cut && self.get("_TRUNCATED").is_none() {
            self.fields.push(Field {
                name: String::from("_TRUNCATED"),
                value: Vec::from(&b"1"[..]),
            });
        }
    }

    /// Add a field a sender gave: one whose name begins with `_` is
    /// dropped, as journald drops it, since only lode writes those.
    pub fn push_untrusted(&mut self, name: &str, value: &[u8]) {
        if !name.starts_with('_') {
            self.push(name, value);
        }
    }

    /// The first value of field `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.fields
            .iter()
            .find(|field| field.name == name)
            .map(|field| field.value.as_slice())
    }

    /// The first value of field `name`, if it is UTF-8.
    #[must_use]
    pub fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name)
            .and_then(|value| core::str::from_utf8(value).ok())
    }

    /// `MESSAGE`, or nothing.
    #[must_use]
    pub fn message(&self) -> &[u8] {
        self.get("MESSAGE").unwrap_or_default()
    }

    /// Who wrote it, as journald's short output names it: the identifier,
    /// else the command, else `unknown`.
    #[must_use]
    pub fn identifier(&self) -> &[u8] {
        self.get("SYSLOG_IDENTIFIER")
            .or_else(|| self.get("_COMM"))
            .unwrap_or(b"unknown")
    }

    /// The writer's pid as its output shows it, if there is one.
    #[must_use]
    pub fn pid(&self) -> Option<&[u8]> {
        self.get("SYSLOG_PID").or_else(|| self.get("_PID"))
    }
}

/// Whether `name` is a field name: 1 to [`NAME_MAX`] bytes of `A-Z`, `0-9`
/// and `_`, not beginning with a digit. `__`-names are the store's own and
/// are never stored as fields.
#[must_use]
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let Some(&first) = bytes.first() else {
        return false;
    };
    bytes.len() <= NAME_MAX
        && !first.is_ascii_digit()
        && !name.starts_with("__")
        && bytes
            .iter()
            .all(|&byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// A level's name, as `-p` takes it and journald spells it.
pub const LEVELS: [&str; 8] = [
    "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
];

/// The level `word` names: a number 0 to 7 or a name (`warn` and `error`
/// as well, as syslog's tools take them).
#[must_use]
pub fn level(word: &str) -> Option<u8> {
    if let Ok(number) = word.parse::<u8>() {
        return (number <= 7).then_some(number);
    }
    let word = match word {
        "warn" => "warning",
        "error" => "err",
        "panic" => "emerg",
        other => other,
    };
    LEVELS
        .iter()
        .position(|name| *name == word)
        .and_then(|index| u8::try_from(index).ok())
}
