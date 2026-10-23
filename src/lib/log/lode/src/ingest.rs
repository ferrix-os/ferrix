//! Records made from what the service reads, and the records it makes
//! about itself (`docs/LOGGING.md` §5, §11.3).

use alloc::vec::Vec;

use crate::console;
use crate::kmsg::KernelRecord;
use crate::record::{LevelSource, Record, UNSPECIFIED};

/// `MESSAGE_ID` of a record saying kernel records were lost.
pub const MESSAGE_ID_LOST: &str = "5b3c2a4e0d6f4a1b9c8e7f6a5d4c3b2a";

/// `MESSAGE_ID` of a record saying the service started after an earlier
/// instance of the same boot ended.
pub const MESSAGE_ID_RESTARTED: &str = "8e1f4b6d2c3a4e5f8091a2b3c4d5e6f7";

/// `MESSAGE_ID` of a record saying the service started for this boot.
pub const MESSAGE_ID_STARTED: &str = "0c7d9e1f2a3b4c5d6e7f8091a2b3c4d5";

/// `MESSAGE_ID` of a kernel record saying a program ended on a fault.
pub const MESSAGE_ID_CRASH: &str = "c4a8e2f60b1d4c3e9f7a5b3d1e9c7a5f";

/// `MESSAGE_ID` of a record saying a segment's torn tail was cut away.
pub const MESSAGE_ID_TORN: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";

/// The clock pair a record's times are worked out from: the wall clock and
/// the monotonic clock read at one moment, in microseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Now {
    /// `CLOCK_REALTIME`.
    pub realtime_us: u64,
    /// `CLOCK_MONOTONIC`, the kernel log's clock.
    pub monotonic_us: u64,
}

impl Now {
    /// The wall-clock time of `monotonic_us`, a moment of this boot.
    #[must_use]
    pub fn realtime_of(&self, monotonic_us: u64) -> u64 {
        self.realtime_us
            .saturating_sub(self.monotonic_us.saturating_sub(monotonic_us))
    }
}

/// A kernel log record as a stored record: the kernel's own lines with
/// `SYSLOG_IDENTIFIER=kernel`, a program's console output with what its
/// text says about who wrote it ([`console::read`]).
#[must_use]
pub fn from_kernel(kernel: &KernelRecord, now: Now) -> Record {
    let mut record = Record::new(kernel.level, now.realtime_of(kernel.usec), kernel.usec);
    let said = console::read(&kernel.text);
    let facility = alloc::format!("{}", kernel.facility);
    let seq = alloc::format!("{}", kernel.seq);
    let usec = alloc::format!("{}", kernel.usec);
    record.push("MESSAGE", &said.message);
    // The kernel's own lines and programs' console output all come at
    // info, which says nothing. A program's `/dev/kmsg` write that named
    // its level is stated: of a facility no console line has (a driver's
    // `<30>`, `LOG_DAEMON`), or `LOG_USER` at another level than the
    // console's info (`<3>`). Any other level the kernel was handed is the
    // kernel's; otherwise the level the line says (a `tracing` WARN), else
    // the one its wording says, marked derived with the words; else none
    // (`docs/LOGGING.md` §4.1).
    if written_with_level(kernel) {
        record.set_level(kernel.level, LevelSource::Stated);
    } else if kernel.level != 6 {
        record.set_level(kernel.level, LevelSource::Kernel);
    } else if let Some(level) = said.level {
        record.set_level(level, LevelSource::Stated);
    } else if let Some((level, word)) = guess_level(&said.message, kernel.facility == 0) {
        record.set_level(level, LevelSource::Derived);
        record.push("FERRIX_PRIORITY_WORD", word.as_bytes());
    } else {
        record.set_level(UNSPECIFIED, LevelSource::Unspecified);
    }
    if kernel.facility == 0 {
        record.push("SYSLOG_IDENTIFIER", b"kernel");
        record.push("_TRANSPORT", b"kernel");
        if let Some(crash) = console::crash(&kernel.text) {
            record.set_level(3, LevelSource::Derived);
            record
                .fields
                .retain(|field| field.name != "FERRIX_PRIORITY_WORD");
            record.push("FERRIX_PRIORITY_WORD", b"signal");
            record.push("MESSAGE_ID", MESSAGE_ID_CRASH.as_bytes());
            record.push("CRASH_PID", alloc::format!("{}", crash.pid).as_bytes());
            record.push("CRASH_SIGNAL", crash.signal.as_bytes());
            if let Some(address) = crash.address {
                record.push("CRASH_ADDRESS", address.as_bytes());
            }
            if let Some(pc) = crash.pc {
                record.push("CRASH_PC", pc.as_bytes());
            }
            if let Some(trap) = crash.trap {
                record.push("CRASH_TRAP", trap.as_bytes());
            }
        }
    } else {
        if let Some(identifier) = &said.identifier {
            record.push("SYSLOG_IDENTIFIER", identifier.as_bytes());
        }
        if let Some(pid) = said.pid {
            record.push("SYSLOG_PID", alloc::format!("{pid}").as_bytes());
        }
        if let Some(unit) = &said.unit {
            record.push("UNIT", unit.as_bytes());
        }
        if let Some(mark) = &said.mark {
            record.push("MARK", mark);
        }
        record.push("_TRANSPORT", b"console");
    }
    // The subsystem a line names, else the program that wrote it: every
    // line that says who wrote it has one, so a viewer can group by it.
    let subsystem = said
        .subsystem
        .as_deref()
        .or(said.identifier.as_deref())
        .or((kernel.facility == 0).then_some("kernel"));
    if let Some(subsystem) = subsystem {
        record.push("FERRIX_SUBSYSTEM", subsystem.as_bytes());
    }
    record.push("SYSLOG_FACILITY", facility.as_bytes());
    record.push("_KERNEL_SEQNUM", seq.as_bytes());
    record.push("_SOURCE_MONOTONIC_TIMESTAMP", usec.as_bytes());
    record
}

/// Whether a kernel log record is a program's `/dev/kmsg` write that named
/// its level: the kernel gives its own lines facility 0, and a program's
/// console output `LOG_USER` at info, so a record of any other facility, or
/// of `LOG_USER` at another level, carries the level its writer wrote. A
/// `<6>` write of `LOG_USER` cannot be told from console output, and is not.
#[must_use]
pub fn written_with_level(kernel: &KernelRecord) -> bool {
    kernel.facility >= 2 || (kernel.facility == 1 && kernel.level != 6)
}

/// A record lode makes about itself, at `priority`.
#[must_use]
pub fn own(priority: u8, message_id: &str, message: &str, now: Now) -> Record {
    let mut record = Record::new(priority, now.realtime_us, now.monotonic_us);
    record.push("MESSAGE", message.as_bytes());
    record.push("MESSAGE_ID", message_id.as_bytes());
    record.push("SYSLOG_IDENTIFIER", b"lode");
    record.push("_TRANSPORT", b"lode");
    record
}

/// What the service knows of the kernel's sequence: the last one stored,
/// and what to say when the next read jumps past it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KernelSequence {
    /// The last kernel sequence number stored this boot, if any.
    pub last: Option<u64>,
}

/// What [`KernelSequence::take`] decided about one kernel record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// Stored already, before a restart: skip it.
    Seen,
    /// New, following on.
    Next,
    /// New, after `lost` records the log no longer had.
    After {
        /// How many.
        lost: u64,
    },
}

impl KernelSequence {
    /// Decide about the record numbered `seq`, and move on past it unless
    /// it was seen.
    pub fn take(&mut self, seq: u64) -> Taken {
        let taken = match self.last {
            Some(last) if seq <= last => return Taken::Seen,
            Some(last) if seq == last + 1 => Taken::Next,
            Some(last) => Taken::After {
                lost: seq - last - 1,
            },
            None if seq == 0 => Taken::Next,
            None => Taken::After { lost: seq },
        };
        self.last = Some(seq);
        taken
    }
}

/// The record that says `lost` kernel records went before the one numbered
/// `seq`, stamped just before it.
#[must_use]
pub fn lost_record(lost: u64, seq: u64, first_start: bool, before: &Record) -> Record {
    let why = if first_start {
        "overwritten before lode started"
    } else {
        "overwritten before lode read them"
    };
    let message = alloc::format!("kernel log: {lost} record(s) before seq {seq} lost, {why}");
    let mut record = Record::new(4, before.realtime_us, before.monotonic_us);
    record.push("MESSAGE", message.as_bytes());
    record.push("MESSAGE_ID", MESSAGE_ID_LOST.as_bytes());
    record.push("SYSLOG_IDENTIFIER", b"lode");
    record.push("_TRANSPORT", b"lode");
    record.push("N_LOST", alloc::format!("{lost}").as_bytes());
    record.push("LOST_BEFORE_SEQNUM", alloc::format!("{seq}").as_bytes());
    record
}

/// The kernel sequence number a stored record carries, if it is a kernel
/// record.
#[must_use]
pub fn kernel_seq(record: &Record) -> Option<u64> {
    record.get_str("_KERNEL_SEQNUM")?.parse().ok()
}

/// The kernel sequence numbers `records` hold for one boot, checked: how
/// many are there, the gaps the store says it lost, and the gaps nothing
/// says, between the first and the last held. What `lode verify --kernel`
/// prints, and what the restart row requires to be empty.
#[must_use]
pub fn kernel_gaps(records: &[&Record]) -> KernelGaps {
    let mut seqs: Vec<u64> = records
        .iter()
        .filter_map(|record| kernel_seq(record))
        .collect();
    seqs.sort_unstable();
    seqs.dedup();
    let mut said: Vec<(u64, u64)> = records
        .iter()
        .filter(|record| record.get_str("MESSAGE_ID") == Some(MESSAGE_ID_LOST))
        .filter_map(|record| {
            let lost: u64 = record.get_str("N_LOST")?.parse().ok()?;
            let before: u64 = record.get_str("LOST_BEFORE_SEQNUM")?.parse().ok()?;
            Some((before.saturating_sub(lost), before))
        })
        .collect();
    said.extend(records.iter().filter_map(|record| {
        let first: u64 = record.get_str("HELD_FIRST_SEQNUM")?.parse().ok()?;
        let last: u64 = record.get_str("HELD_LAST_SEQNUM")?.parse().ok()?;
        Some((first, last + 1))
    }));
    said.sort_unstable();
    let mut unsaid = Vec::new();
    for pair in seqs.windows(2) {
        let &[low, high] = pair else {
            continue;
        };
        if high > low + 1 {
            // Every missing number must be in some record's range; ranges
            // that meet or overlap cover a gap together.
            let mut next = low + 1;
            for &(from, to) in &said {
                if from <= next && to > next {
                    next = to;
                }
            }
            if next < high {
                unsaid.push((low + 1, high));
            }
        }
    }
    KernelGaps {
        held: seqs.len(),
        first: seqs.first().copied(),
        last: seqs.last().copied(),
        said_lost: records
            .iter()
            .filter(|record| record.get_str("MESSAGE_ID") == Some(MESSAGE_ID_LOST))
            .filter_map(|record| record.get_str("N_LOST")?.parse::<u64>().ok())
            .sum(),
        unsaid,
    }
}

/// [`kernel_gaps`]' answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelGaps {
    /// Kernel records held.
    pub held: usize,
    /// The first sequence number held.
    pub first: Option<u64>,
    /// The last.
    pub last: Option<u64>,
    /// Records the store says it lost.
    pub said_lost: u64,
    /// Gaps, `from..to`, no record accounts for.
    pub unsaid: Vec<(u64, u64)>,
}

/// The level a line's wording says, and the words that said it: `failed`,
/// `could not`, `cannot`, `unable` and `error:` an error; `refused`,
/// `denied`, `lost`, `dropped`, `timed out` and `warning` a warning --
/// none of them after `0` or `no`, which is a count of nothing
/// (`0 refused`); `FERRIX-PANIC` critical. The kernel's own lines report
/// what its boot checks refused on purpose, so for `kernel` only the error
/// words and an upper-case `WARNING:` count.
#[must_use]
pub fn guess_level(message: &[u8], kernel: bool) -> Option<(u8, &'static str)> {
    let text = core::str::from_utf8(message).ok()?;
    if text.contains("FERRIX-PANIC") {
        return Some((2, "FERRIX-PANIC"));
    }
    let shouted = text.contains("WARNING:");
    let lower = text.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == ':'))
        .filter(|word| !word.is_empty())
        .collect();
    // `0 lost`, `no packets dropped`, `0 rows refused`: a count of none,
    // one or two words before.
    let counted = |index: usize| {
        (1..=2).any(|back| {
            index
                .checked_sub(back)
                .and_then(|before| words.get(before))
                .is_some_and(|before| *before == "0" || *before == "no")
        })
    };
    let mut found: Option<(u8, &'static str)> = None;
    for (index, word) in words.iter().enumerate() {
        let next = words.get(index + 1).copied().unwrap_or_default();
        let hit: Option<(u8, &'static str)> = match (*word, next) {
            ("failed" | "failed:", _) => Some((3, "failed")),
            ("could", "not") => Some((3, "could not")),
            ("cannot", _) => Some((3, "cannot")),
            ("unable", _) => Some((3, "unable")),
            ("error:", _) => Some((3, "error")),
            ("refused" | "refused:", _) => Some((4, "refused")),
            ("denied", _) => Some((4, "denied")),
            ("lost", _) => Some((4, "lost")),
            ("dropped", _) => Some((4, "dropped")),
            ("timed", "out") => Some((4, "timed out")),
            ("warning" | "warning:", _) => Some((4, "warning")),
            _ => None,
        };
        let hit = hit.filter(|(level, _)| !kernel || *level <= 3);
        if let Some(hit) = hit
            && !counted(index)
            && found.is_none_or(|(level, _)| hit.0 < level)
        {
            found = Some(hit);
        }
    }
    found.or_else(|| shouted.then_some((4, "WARNING:")))
}
