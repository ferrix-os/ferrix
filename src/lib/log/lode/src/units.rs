//! Units' standard output and error as init hands them to lode
//! (`docs/LOGGING.md` §4.2, §5).
//!
//! init makes each spawn's pipes, one for standard output and one for
//! standard error, so it knows without asking anyone which unit, which
//! invocation of it, which process and which stream a line came from. It
//! connects to lode's [`SOCKET`], which lode makes as root, mode 0600, and
//! refuses to any peer but root: what init says there is trusted, and a
//! line becomes a record with `_SYSTEMD_UNIT`, `_SYSTEMD_INVOCATION_ID`,
//! `_STREAM`, `_PID`, `_UID` and `_GID`. One line on the socket is one
//! line a unit wrote:
//!
//! ```text
//! L <unit> <invocation, 32 hex digits, or -> <pid> <uid> <gid> <out|err> <monotonic µs> <text>\n
//! ```
//!
//! init also goes on writing every line to the console as before (`  init
//! NAME[PID]: TEXT`), which is how gates read a unit's lines and how a line
//! reaches the log while lode is not there. lode reads that copy in the
//! kernel log too, so it pairs the two ([`Pairing`]): the trusted record
//! takes the copy's kernel sequence number and the copy is not stored, and
//! a copy init did not hand over -- lode was down, or init's socket was full
//! -- is stored as before, with `UNIT` from its text and no trusted fields.

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ingest::{self, Now};
use crate::kmsg::KernelRecord;
use crate::record::Record;
use crate::segment;

/// Where lode listens for init.
pub const SOCKET: &str = "/run/lode/units";

/// How long a trusted line waits for its console copy before it is stored
/// alone, and how long an untrusted copy stored alone is remembered, so a
/// trusted line arriving after it is not stored twice.
pub const PAIR_US: u64 = 2_000_000;

/// The most lines either side of a [`Pairing`] holds.
const HELD_MAX: usize = 4096;

/// Which of a process's streams a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Out,
    /// Standard error.
    Err,
}

impl Stream {
    /// As the socket and `_STREAM` say it.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Stream::Out => "stdout",
            Stream::Err => "stderr",
        }
    }
}

/// One line a unit wrote, as init hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitLine {
    /// The unit, as init names it.
    pub unit: String,
    /// The invocation: drawn by init each time the unit starts.
    pub invocation: Option<[u8; 16]>,
    /// The process init started, whose pipe this was.
    pub pid: u32,
    /// The process's user.
    pub uid: u32,
    /// The process's group.
    pub gid: u32,
    /// Which stream.
    pub stream: Stream,
    /// When init read it, on the monotonic clock, in microseconds.
    pub monotonic_us: u64,
    /// The line, without its newline.
    pub text: Vec<u8>,
}

/// `line` as init writes it to the socket, appended to `out`. A newline in
/// the text (there is none: init splits at them) is written as a space.
pub fn encode(line: &UnitLine, out: &mut Vec<u8>) {
    let mut invocation = String::new();
    match &line.invocation {
        Some(id) => segment::push_hex(&mut invocation, id),
        None => invocation.push('-'),
    }
    out.extend_from_slice(
        alloc::format!(
            "L {} {invocation} {} {} {} {} {} ",
            line.unit,
            line.pid,
            line.uid,
            line.gid,
            match line.stream {
                Stream::Out => "out",
                Stream::Err => "err",
            },
            line.monotonic_us
        )
        .as_bytes(),
    );
    out.extend(
        line.text
            .iter()
            .map(|&byte| if byte == b'\n' { b' ' } else { byte }),
    );
    out.push(b'\n');
}

/// One line of the socket, without its newline; `None` for one that does
/// not read whole.
#[must_use]
pub fn decode(line: &[u8]) -> Option<UnitLine> {
    let mut rest = line.strip_prefix(b"L ")?;
    let mut word = || -> Option<&str> {
        let end = rest.iter().position(|&byte| byte == b' ')?;
        let taken = core::str::from_utf8(rest.get(..end)?).ok()?;
        rest = rest.get(end + 1..)?;
        Some(taken)
    };
    let unit = word()?;
    if unit.is_empty() {
        return None;
    }
    let unit = String::from(unit);
    let invocation = match word()? {
        "-" => None,
        hex => Some(segment::parse_id(hex).filter(|_| hex.len() == 32)?),
    };
    let pid = word()?.parse().ok()?;
    let uid = word()?.parse().ok()?;
    let gid = word()?.parse().ok()?;
    let stream = match word()? {
        "out" => Stream::Out,
        "err" => Stream::Err,
        _ => return None,
    };
    let monotonic_us = word()?.parse().ok()?;
    Some(UnitLine {
        unit,
        invocation,
        pid,
        uid,
        gid,
        stream,
        monotonic_us,
        text: Vec::from(rest),
    })
}

/// What init writes to the console for `line`: `  init     NAME[PID]: TEXT`.
#[must_use]
pub fn console_text(line: &UnitLine) -> Vec<u8> {
    let mut text = Vec::from(alloc::format!("  init     {}[{}]: ", line.unit, line.pid).as_bytes());
    text.extend_from_slice(&line.text);
    text
}

/// The record `line` makes: its console copy read as the kernel log's
/// lines are (identifier, level from its words), then the fields only init
/// can vouch for. It has no kernel sequence number until it is paired.
#[must_use]
pub fn record(line: &UnitLine, now: Now) -> Record {
    let kernel = KernelRecord {
        facility: 1,
        level: 6,
        seq: 0,
        usec: line.monotonic_us,
        text: console_text(line),
    };
    let mut record = ingest::from_kernel(&kernel, now);
    record.fields.retain(|field| {
        !matches!(
            field.name.as_str(),
            "_KERNEL_SEQNUM" | "_TRANSPORT" | "SYSLOG_FACILITY"
        )
    });
    record.push("_TRANSPORT", b"stdout");
    record.push("_SYSTEMD_UNIT", line.unit.as_bytes());
    if let Some(id) = &line.invocation {
        let mut hex = String::new();
        segment::push_hex(&mut hex, id);
        record.push("_SYSTEMD_INVOCATION_ID", hex.as_bytes());
    }
    record.push("_STREAM", line.stream.word().as_bytes());
    record.push("_PID", alloc::format!("{}", line.pid).as_bytes());
    record.push("_UID", alloc::format!("{}", line.uid).as_bytes());
    record.push("_GID", alloc::format!("{}", line.gid).as_bytes());
    record
}

/// Whether `console`, a console copy read from the kernel log, is the copy
/// of `trusted`: the same unit, pid and message. A long line the kernel
/// cut is the copy of the line it begins.
fn is_copy(console: &Record, trusted: &Record) -> bool {
    let same = |name: &str| console.get(name).is_some() && console.get(name) == trusted.get(name);
    if !same("UNIT") || !same("SYSLOG_PID") {
        return false;
    }
    let (copy, line) = (console.message(), trusted.message());
    copy == line || (copy.len() >= 200 && line.starts_with(copy))
}

/// Whether `record`, read from the kernel log, is a unit's line init wrote
/// to the console, which init may also have handed over.
#[must_use]
pub fn is_unit_copy(record: &Record) -> bool {
    record.get("_TRANSPORT") == Some(b"console")
        && record.get("UNIT").is_some()
        && record.get("SYSLOG_PID").is_some()
}

/// Trusted lines waiting for their console copies, and copies stored
/// alone, which a late trusted line must not repeat.
#[derive(Debug, Default)]
pub struct Pairing {
    /// Trusted records not yet paired, with when they arrived.
    waiting: VecDeque<(Record, u64)>,
    /// Console copies stored without a trusted line, with when.
    alone: VecDeque<(Record, u64)>,
    /// Trusted lines dropped because their copy was stored alone before
    /// them: their content is kept, their trusted fields are not.
    pub late: u64,
}

impl Pairing {
    /// A trusted record from init, arrived at `now`. Anything that must be
    /// stored at once -- the oldest waiting record when too many wait --
    /// goes into `out`.
    pub fn trusted(&mut self, record: Record, now: u64, out: &mut Vec<Record>) {
        if let Some(at) = self
            .alone
            .iter()
            .position(|(copy, _)| is_copy(copy, &record))
        {
            let _ = self.alone.remove(at);
            self.late += 1;
            return;
        }
        if self.waiting.len() >= HELD_MAX
            && let Some((oldest, _)) = self.waiting.pop_front()
        {
            out.push(oldest);
        }
        self.waiting.push_back((record, now));
    }

    /// A console copy read from the kernel log: the trusted record it
    /// pairs with, given the copy's kernel sequence number and stamps, or
    /// the copy back when no trusted line waits for it.
    pub fn console(&mut self, copy: Record) -> Result<Record, Record> {
        let Some(at) = self
            .waiting
            .iter()
            .position(|(trusted, _)| is_copy(&copy, trusted))
        else {
            return Err(copy);
        };
        let Some((mut trusted, _)) = self.waiting.remove(at) else {
            return Err(copy);
        };
        trusted.realtime_us = copy.realtime_us;
        trusted.monotonic_us = copy.monotonic_us;
        if let Some(seq) = copy.get("_KERNEL_SEQNUM") {
            trusted.push("_KERNEL_SEQNUM", seq);
        }
        Ok(trusted)
    }

    /// A console copy is stored alone, at `now`: a trusted line for it that
    /// comes later is not stored again.
    pub fn stored_alone(&mut self, copy: &Record, now: u64) {
        if self.alone.len() >= HELD_MAX {
            let _ = self.alone.pop_front();
        }
        self.alone.push_back((copy.clone(), now));
    }

    /// Trusted records that waited [`PAIR_US`] for a copy that did not come
    /// (the kernel log lost it, or the console was full): stored without a
    /// kernel sequence number. Copies remembered that long are forgotten.
    pub fn expire(&mut self, now: u64, out: &mut Vec<Record>) {
        while let Some((_, at)) = self.waiting.front() {
            if now.saturating_sub(*at) < PAIR_US {
                break;
            }
            if let Some((record, _)) = self.waiting.pop_front() {
                out.push(record);
            }
        }
        while self
            .alone
            .front()
            .is_some_and(|(_, at)| now.saturating_sub(*at) >= PAIR_US)
        {
            let _ = self.alone.pop_front();
        }
    }

    /// Whether any trusted record waits.
    #[must_use]
    pub fn is_waiting(&self) -> bool {
        !self.waiting.is_empty()
    }
}
