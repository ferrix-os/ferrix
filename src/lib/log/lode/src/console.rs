//! Who wrote a line the console carried, as far as its text says.
//!
//! Until services and drivers log to lode directly (`docs/LOGGING.md` steps
//! 4 and 7), what they say reaches it through the console, which is the
//! kernel log: init writes a unit's lines as `  init     NAME[PID]: TEXT`,
//! the kernel's own subsystems as `  name     text`, and a driver's standard
//! error as whatever it prints, often `name: text`. These are read into
//! fields here. Anything can write to the console, so none of them is a
//! trusted (`_`) field: a unit named here is `UNIT`, not `_SYSTEMD_UNIT`.

use alloc::string::String;
use alloc::vec::Vec;

/// What a console line says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Said {
    /// The subsystem or program word a `  word     text` line begins with.
    pub subsystem: Option<String>,
    /// Who wrote it.
    pub identifier: Option<String>,
    /// Its writer's pid, where the line names one.
    pub pid: Option<u32>,
    /// The unit init named.
    pub unit: Option<String>,
    /// A `lode mark` line's text.
    pub mark: Option<Vec<u8>>,
    /// The level the line itself says, where it says one.
    pub level: Option<u8>,
    /// The message: the line without what was read off it.
    pub message: Vec<u8>,
}

/// What `lode mark TEXT` writes to the kernel log, before the text.
pub const MARK_PREFIX: &[u8] = b"lode-mark: ";

/// Read `raw`, a kernel record's text: ANSI colour sequences taken out
/// first, since a terminal's colours are not the message.
#[must_use]
pub fn read(raw: &[u8]) -> Said {
    let cleaned = strip_ansi(raw);
    let line = cleaned.as_slice();
    if let Some(mark) = line.strip_prefix(MARK_PREFIX) {
        return Said {
            identifier: Some(String::from("lode-mark")),
            mark: Some(Vec::from(mark)),
            message: Vec::from(line),
            ..Said::default()
        };
    }
    if let Some(said) = tracing_line(line) {
        return said;
    }
    if let Some((word, rest)) = padded_word(line) {
        if word == "init"
            && let Some(said) = unit_line(rest)
        {
            return said;
        }
        return Said {
            subsystem: Some(String::from(word)),
            identifier: Some(String::from(word)),
            pid: (word == "init").then_some(1),
            message: Vec::from(trim_start(line)),
            ..Said::default()
        };
    }
    if let Some((word, rest)) = colon_word(line) {
        return Said {
            identifier: Some(String::from(word)),
            message: Vec::from(trim_start(rest)),
            ..Said::default()
        };
    }
    Said {
        message: Vec::from(line),
        ..Said::default()
    }
}

/// `bytes` without ANSI escape sequences (`ESC [ ... letter`, and a lone
/// `ESC` with the byte after it).
#[must_use]
pub fn strip_ansi(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while let Some(&byte) = bytes.get(at) {
        if byte != 0x1b {
            out.push(byte);
            at += 1;
        } else if bytes.get(at + 1) == Some(&b'[') {
            // `ESC [`, parameters, and the letter that ends it.
            let rest = bytes.get(at + 2..).unwrap_or_default();
            at += 2 + rest
                .iter()
                .position(u8::is_ascii_alphabetic)
                .map_or(rest.len(), |end| end + 1);
        } else {
            at += 2;
        }
    }
    out
}

/// A `tracing` line, as Rust programs write them to standard error:
/// `2026-10-23T17:21:51.786662Z  WARN crate::module: message`. The level is
/// the program's own, and the identifier the crate.
fn tracing_line(line: &[u8]) -> Option<Said> {
    let text = core::str::from_utf8(line).ok()?;
    let (stamp, rest) = text.split_once(' ')?;
    if stamp.len() < 20 || !stamp.ends_with('Z') || stamp.as_bytes().get(10) != Some(&b'T') {
        return None;
    }
    let rest = rest.trim_start();
    let (level, rest) = rest.split_once(' ')?;
    let level = match level {
        "ERROR" => 3,
        "WARN" => 4,
        "INFO" => 6,
        "DEBUG" | "TRACE" => 7,
        _ => return None,
    };
    let (target, message) = rest.trim_start().split_once(": ")?;
    let crate_name = target.split("::").next().filter(|name| !name.is_empty())?;
    Some(Said {
        identifier: Some(String::from(crate_name)),
        level: Some(level),
        message: Vec::from(message.as_bytes()),
        ..Said::default()
    })
}

/// `word   rest`: a word of a letter and then letters, digits, `_`, `-` or
/// `.`, padded with spaces to the kernel's column (nine) or by two or more,
/// with or without the two spaces the kernel's own lines begin with --
/// `  devmgr   started`, `devmgr   intel ... started` from `devmgr`'s
/// standard error.
fn padded_word(line: &[u8]) -> Option<(&str, &[u8])> {
    let lead = line.iter().position(|&byte| byte != b' ')?;
    let body = line.get(lead..)?;
    let end = body.iter().position(|&byte| !is_word(byte))?;
    if end == 0 || end > 16 || !body.first()?.is_ascii_alphabetic() {
        return None;
    }
    let spaces = body
        .get(end..)?
        .iter()
        .take_while(|&&byte| byte == b' ')
        .count();
    let rest = body.get(end + spaces..)?;
    if spaces == 0 || rest.is_empty() || (spaces < 2 && end + spaces < 9) {
        return None;
    }
    let word = core::str::from_utf8(body.get(..end)?).ok()?;
    Some((word, rest))
}

/// `NAME[PID]: TEXT`, init's line for a unit.
fn unit_line(rest: &[u8]) -> Option<Said> {
    let open = rest.iter().position(|&byte| byte == b'[')?;
    let close = open + rest.get(open..)?.iter().position(|&byte| byte == b']')?;
    let name = core::str::from_utf8(rest.get(..open)?).ok()?;
    if name.is_empty() || !name.bytes().all(|byte| is_word(byte) || byte == b'@') {
        return None;
    }
    let pid: u32 = core::str::from_utf8(rest.get(open + 1..close)?)
        .ok()?
        .parse()
        .ok()?;
    let text = rest.get(close + 1..)?.strip_prefix(b":")?;
    let identifier = name
        .strip_suffix(".service")
        .or_else(|| name.strip_suffix(".socket"))
        .unwrap_or(name);
    Some(Said {
        subsystem: Some(String::from("init")),
        identifier: Some(String::from(identifier)),
        pid: Some(pid),
        unit: Some(String::from(name)),
        mark: None,
        level: None,
        message: Vec::from(text.strip_prefix(b" ").unwrap_or(text)),
    })
}

/// `word: rest`, a word of at most 32 bytes beginning with a letter. A
/// word with digits glued before it (`0usbhid`, `0xusbhid`: the tail of a
/// number another writer left on the console with no newline) is read
/// without them.
fn colon_word(line: &[u8]) -> Option<(&str, &[u8])> {
    let end = line.iter().position(|&byte| !is_word(byte))?;
    if end == 0 || end > 32 || line.get(end) != Some(&b':') || line.get(end + 1) != Some(&b' ') {
        return None;
    }
    let word = core::str::from_utf8(line.get(..end)?).ok()?;
    let start = word.find(|c: char| c.is_ascii_alphabetic())?;
    let glued = word.get(..start)?;
    let word = if glued.is_empty() {
        word
    } else if glued.bytes().all(|byte| byte.is_ascii_digit()) {
        word.get(start..)?
    } else {
        return None;
    };
    let word = word
        .strip_prefix('x')
        .filter(|_| glued == "0")
        .unwrap_or(word);
    // `error: ...` and `WARNING: ...` name a level, not a program.
    let level_word = [
        "error", "warning", "warn", "info", "debug", "note", "fatal", "panic",
    ]
    .iter()
    .any(|level| word.eq_ignore_ascii_case(level));
    (word.len() >= 2 && word.starts_with(|c: char| c.is_ascii_alphabetic()) && !level_word)
        .then_some((word, line.get(end + 1..)?))
}

/// Bytes a subsystem or program name is made of.
fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

/// `bytes` without its leading spaces.
fn trim_start(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|&byte| byte != b' ')
        .unwrap_or(bytes.len());
    bytes.get(start..).unwrap_or_default()
}

/// A program the kernel ended on a fault, as its `signal` line says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Crash {
    /// The process.
    pub pid: u32,
    /// The signal's name (`SIGSEGV`), or its number where it has none.
    pub signal: String,
    /// The faulting address, as printed.
    pub address: Option<String>,
    /// The program counter, as printed.
    pub pc: Option<String>,
    /// The trap, as the kernel names it.
    pub trap: Option<String>,
}

/// Read a kernel `signal` line that ends a process:
/// `  signal   pid P ended by signal S at ADDRESS, pc PC: TRAP`
/// (`src/kernel/src/trap.rs`), or `pid P has no room for signal S's frame;
/// ending it with SIGSEGV` and `pid P returned from a handler through a bad
/// frame; ending it with SIGSEGV` (`syscall/deliver.rs`).
#[must_use]
pub fn crash(line: &[u8]) -> Option<Crash> {
    let (word, rest) = padded_word(line)?;
    if word != "signal" {
        return None;
    }
    let text = core::str::from_utf8(rest).ok()?;
    let rest = text.strip_prefix("pid ")?;
    let (pid, rest) = rest.split_once(' ')?;
    let pid: u32 = pid.parse().ok()?;
    if let Some(rest) = rest.strip_prefix("ended by signal ") {
        let (number, rest) = rest.split_once(" at ")?;
        let (address, rest) = rest.split_once(", pc ")?;
        let (pc, trap) = rest.split_once(": ").unwrap_or((rest, ""));
        return Some(Crash {
            pid,
            signal: signal_name(number),
            address: Some(String::from(address)),
            pc: Some(String::from(pc)),
            trap: (!trap.is_empty()).then(|| String::from(trap)),
        });
    }
    rest.ends_with("ending it with SIGSEGV").then(|| Crash {
        pid,
        signal: String::from("SIGSEGV"),
        address: None,
        pc: None,
        trap: Some(String::from(rest)),
    })
}

/// A signal number's name, Linux's numbering, the same on every
/// architecture Ferrix builds for for these.
fn signal_name(number: &str) -> String {
    let name = match number.trim() {
        "4" => "SIGILL",
        "5" => "SIGTRAP",
        "6" => "SIGABRT",
        "7" => "SIGBUS",
        "8" => "SIGFPE",
        "11" => "SIGSEGV",
        "31" => "SIGSYS",
        other => return String::from(other),
    };
    String::from(name)
}
