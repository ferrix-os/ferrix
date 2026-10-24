//! Tombstones (`docs/LOGGING.md` §21): one lean record a crash that says
//! what is known of it and links to the rest -- the crash record, the
//! crashed process's last lines, its session, the boot's image -- and,
//! from the collector, references to the heavy parts in the blob container.
//!
//! A crash storm is cut short: per signature (program, module and offset
//! of the pc, signal) a boot gets [`Tombstones::full`] tombstones, then a
//! lean repeat record each time the count doubles, so 146 crashes of one
//! helper are five tombstones and a handful of counters.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::string::String;
use alloc::vec::Vec;

use crate::ingest::{self, Now};
use crate::record::Record;
use crate::segment::push_hex;

/// `MESSAGE_ID` of a tombstone.
pub const MESSAGE_ID_TOMBSTONE: &str = "70b5f0e1d2c34a5b9c8d7e6f5a4b3c2d";

/// `MESSAGE_ID` of a crash storm's repeat record.
pub const MESSAGE_ID_REPEAT: &str = "70b5f0e1d2c34a5b9c8d7e6f5a4b3c2e";

/// How many of a process's last records a tombstone's tail spans.
pub const TAIL: usize = 50;

/// The most processes whose last records are kept track of.
const PIDS_MAX: usize = 512;

/// A cursor from a sequence number and a boot.
#[must_use]
pub fn cursor(seqnum: u64, boot: &[u8; 16]) -> String {
    let mut text = alloc::format!("i={seqnum:x};b=");
    push_hex(&mut text, boot);
    text
}

/// A crash's signature: its program, the module and offset of its pc, and
/// its signal.
#[must_use]
pub fn signature(crash: &Record) -> String {
    let field = |name: &str| crash.get_str(name).unwrap_or("?");
    alloc::format!(
        "{} {} {}",
        field("CRASH_PROGRAM"),
        field("CRASH_MODULE"),
        field("CRASH_SIGNAL")
    )
}

/// What a signature has had this boot.
#[derive(Debug, Clone)]
struct Seen {
    /// Crashes.
    count: u64,
    /// The newest full tombstone's crash's sequence number.
    last_full: u64,
    /// The count at the last repeat record.
    said: u64,
}

/// The tombstones of a boot.
#[derive(Debug)]
pub struct Tombstones {
    /// Full tombstones a signature gets a boot.
    pub full: u64,
    /// Per signature.
    seen: BTreeMap<String, Seen>,
    /// Per pid, the sequence numbers of its last [`TAIL`] records.
    tails: VecDeque<(u32, VecDeque<u64>)>,
}

impl Tombstones {
    /// `full` tombstones a signature a boot.
    #[must_use]
    pub fn new(full: u64) -> Tombstones {
        Tombstones {
            full,
            seen: BTreeMap::new(),
            tails: VecDeque::new(),
        }
    }

    /// After a restart, a tombstone or repeat record this boot already
    /// stored, so a storm's counts go on where they were.
    pub fn restore(&mut self, record: &Record) {
        let (Some(key), Some(count)) = (
            record.get_str("TOMBSTONE_SIGNATURE"),
            record
                .get_str("TOMBSTONE_REPEATS")
                .and_then(|count| count.parse::<u64>().ok()),
        ) else {
            return;
        };
        let full = record.get("TOMBSTONE").is_some();
        let of = record
            .get_str(if full {
                "TOMBSTONE_OF"
            } else {
                "TOMBSTONE_FIRST"
            })
            .and_then(crate::cursor::seqnum)
            .unwrap_or(0);
        let seen = self.seen.entry(String::from(key)).or_insert(Seen {
            count: 0,
            last_full: of,
            said: 0,
        });
        if count >= seen.count {
            seen.count = count;
            seen.last_full = of;
            if !full {
                seen.said = count;
            }
        }
    }

    /// A record of process `pid` was stored as `seqnum`.
    pub fn stored(&mut self, pid: u32, seqnum: u64) {
        let at = match self.tails.iter().position(|(each, _)| *each == pid) {
            Some(at) => at,
            None => {
                if self.tails.len() >= PIDS_MAX {
                    let _ = self.tails.pop_front();
                }
                self.tails.push_back((pid, VecDeque::new()));
                self.tails.len() - 1
            }
        };
        if let Some((_, tail)) = self.tails.get_mut(at) {
            if tail.len() >= TAIL {
                let _ = tail.pop_front();
            }
            tail.push_back(seqnum);
        }
    }

    /// The tombstone for `crash`, stored as `seqnum` in `boot`, or its
    /// repeat record, or nothing (a repeat between two said counts).
    pub fn for_crash(
        &mut self,
        crash: &Record,
        seqnum: u64,
        boot: &[u8; 16],
        now: Now,
    ) -> Option<Record> {
        let key = signature(crash);
        let full = self.full;
        let seen = self.seen.entry(key.clone()).or_insert(Seen {
            count: 0,
            last_full: seqnum,
            said: 0,
        });
        seen.count += 1;
        let pid: Option<u32> = crash.get_str("CRASH_PID").and_then(|pid| pid.parse().ok());
        if seen.count <= full {
            seen.last_full = seqnum;
            let count = seen.count;
            let tail = pid.and_then(|pid| {
                self.tails
                    .iter()
                    .find(|(each, _)| *each == pid)
                    .map(|(_, tail)| (tail.front().copied(), tail.back().copied()))
            });
            return Some(full_record(crash, seqnum, boot, now, count, tail));
        }
        if seen.count < seen.said.max(full) * 2 {
            return None;
        }
        seen.said = seen.count;
        let comm = crash.get_str("CRASH_COMM").unwrap_or("?");
        let mut record = ingest::own(
            4,
            MESSAGE_ID_REPEAT,
            &alloc::format!(
                "{comm} crashed again, {} times this boot ({key}); tombstones for the first {full}",
                seen.count
            ),
            now,
        );
        record.push(
            "TOMBSTONE_REPEATS",
            alloc::format!("{}", seen.count).as_bytes(),
        );
        record.push("TOMBSTONE_SIGNATURE", key.as_bytes());
        record.push("TOMBSTONE_FIRST", cursor(seen.last_full, boot).as_bytes());
        record.push("TOMBSTONE_LAST", cursor(seqnum, boot).as_bytes());
        Some(record)
    }
}

/// A full tombstone: the crash's own fields renamed, the links, the
/// count.
fn full_record(
    crash: &Record,
    seqnum: u64,
    boot: &[u8; 16],
    now: Now,
    count: u64,
    tail: Option<(Option<u64>, Option<u64>)>,
) -> Record {
    let comm = crash.get_str("CRASH_COMM").unwrap_or("?");
    let signal = crash.get_str("CRASH_SIGNAL").unwrap_or("?");
    let module = crash.get_str("CRASH_MODULE").unwrap_or("?");
    let mut record = ingest::own(
        3,
        MESSAGE_ID_TOMBSTONE,
        &alloc::format!("tombstone: {comm} ended by {signal} at {module}"),
        now,
    );
    record.push("TOMBSTONE", b"1");
    for (from, to) in [
        ("CRASH_PID", "TOMBSTONE_PID"),
        ("CRASH_COMM", "TOMBSTONE_COMM"),
        ("CRASH_EXE", "TOMBSTONE_EXE"),
        ("CRASH_CMDLINE", "TOMBSTONE_CMDLINE"),
        ("CRASH_SIGNAL", "TOMBSTONE_SIGNAL"),
        ("CRASH_ADDRESS", "TOMBSTONE_ADDRESS"),
        ("CRASH_PC", "TOMBSTONE_PC"),
        ("CRASH_TRAP", "TOMBSTONE_TRAP"),
        ("CRASH_MODULE", "TOMBSTONE_MODULE"),
        ("_SYSTEMD_UNIT", "TOMBSTONE_UNIT"),
        ("SESSION_ID", "TOMBSTONE_SESSION"),
    ] {
        if let Some(value) = crash.get(from) {
            record.push(to, value);
        }
    }
    // Without a collector's backtrace, the top frame is where the pc was.
    if let Some(module) = crash.get("CRASH_MODULE") {
        record.push("TOMBSTONE_TOP_FRAME", module);
    }
    record.push("TOMBSTONE_SIGNATURE", signature(crash).as_bytes());
    record.push("TOMBSTONE_OF", cursor(seqnum, boot).as_bytes());
    record.push("TOMBSTONE_REPEATS", alloc::format!("{count}").as_bytes());
    if let Some((Some(first), Some(last))) = tail {
        record.push("TOMBSTONE_TAIL_FIRST", cursor(first, boot).as_bytes());
        record.push("TOMBSTONE_TAIL_LAST", cursor(last, boot).as_bytes());
    }
    record
}

/// Whether `record` is a tombstone or a storm's repeat record.
#[must_use]
pub fn is_tombstone(record: &Record) -> bool {
    matches!(
        record.get_str("MESSAGE_ID"),
        Some(MESSAGE_ID_TOMBSTONE | MESSAGE_ID_REPEAT)
    )
}

/// The heavy parts a collector found, as the record fields and blobs they
/// become ([`crate::blob::part`]): name, type, bytes.
#[must_use]
pub fn parts(
    parts: Vec<(&str, &str, Vec<u8>)>,
) -> (Vec<crate::blob::Field>, Vec<crate::blob::Pending>) {
    let mut fields = Vec::new();
    let mut blobs = Vec::new();
    for (name, kind, bytes) in parts {
        let (field, blob) = crate::blob::part(name, kind, bytes);
        fields.push(field);
        blobs.extend(blob);
    }
    (fields, blobs)
}
