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
    /// The message: the line without what was read off it.
    pub message: Vec<u8>,
}

/// What `lode mark TEXT` writes to the kernel log, before the text.
pub const MARK_PREFIX: &[u8] = b"lode-mark: ";

/// Read `line`, a kernel record's text.
#[must_use]
pub fn read(line: &[u8]) -> Said {
    if let Some(mark) = line.strip_prefix(MARK_PREFIX) {
        return Said {
            identifier: Some(String::from("lode-mark")),
            mark: Some(Vec::from(mark)),
            message: Vec::from(line),
            ..Said::default()
        };
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

/// `  word   rest`: two spaces, a word, at least one space.
fn padded_word(line: &[u8]) -> Option<(&str, &[u8])> {
    let body = line.strip_prefix(b"  ")?;
    let end = body.iter().position(|&byte| !is_word(byte))?;
    if end == 0 || body.get(end) != Some(&b' ') {
        return None;
    }
    let word = core::str::from_utf8(body.get(..end)?).ok()?;
    Some((word, trim_start(body.get(end..)?)))
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
        message: Vec::from(text.strip_prefix(b" ").unwrap_or(text)),
    })
}

/// `word: rest`, a word of at most 32 bytes.
fn colon_word(line: &[u8]) -> Option<(&str, &[u8])> {
    let end = line.iter().position(|&byte| !is_word(byte))?;
    if end == 0 || end > 32 || line.get(end) != Some(&b':') || line.get(end + 1) != Some(&b' ') {
        return None;
    }
    let word = core::str::from_utf8(line.get(..end)?).ok()?;
    Some((word, line.get(end + 1..)?))
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
