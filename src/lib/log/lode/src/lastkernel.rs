//! The previous boot's end, from the loader's `FERRIX/LASTKERNEL.TXT`
//! (`docs/LOGGING.md` §5, `docs/POSTMORTEM.md` §3.5): what is a pure function
//! of bytes.
//!
//! The loader writes the file at a boot that found the previous kernel's
//! log in the pstore region: its report lines (`  pstore   at ..., 1 MiB:
//! boot N's kernel log, W bytes, K kept (unchecked)`, the last report, how
//! it ended), then the region's whole text. lode imports the lines of it that
//! it did not store, as records of that boot (`_TRANSPORT=pstore`), so that
//! `lode -b -1` ends with what the machine said before it hung or panicked.
//!
//! Which boot: the kernel says once each boot `pstore   mirroring the log to
//! ADDRESS, 1024 KiB, boot N;`, which lode stores as a kernel record; the
//! file is boot N's, and it is imported only into the boot lode holds that
//! said so. Which lines: the region holds the kernel log from the boot's
//! start, the same lines `/dev/kmsg` handed lode; lode's last few stored
//! kernel lines are found in it, together and in order, searching from its
//! end, and what follows them is new. A file whose text holds none of them is
//! not imported, rather than doubled.

use alloc::string::String;
use alloc::vec::Vec;

use crate::ingest::{self, Now};
use crate::record::Record;

/// Where the loader keeps it on its volume.
pub const PATH: &str = "FERRIX/LASTKERNEL.TXT";

/// The most the file may be: the region's ring and the report's lines.
pub const MOST: usize = 1024 * 1024 + 1024;

/// The import's own record's `MESSAGE_ID`.
pub const MESSAGE_ID_IMPORTED: &str = "4f6c1b2d9a8e4c37b5d0e1f2a3b4c5d6";

/// How many of lode's last stored lines must be found together.
const RUN: usize = 3;

/// The file, read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept<'a> {
    /// The pstore boot number it is of.
    pub sequence: u64,
    /// Whether the region had wrapped, so its first line is a piece.
    pub cut: bool,
    /// Its text's whole lines, oldest first.
    pub lines: Vec<&'a str>,
    /// The loader's report lines that head it.
    pub head: Vec<&'a str>,
}

/// The number after `before` in `text`, up to `after`.
fn number_between(text: &str, before: &str, after: &str) -> Option<u64> {
    let (_, rest) = text.split_once(before)?;
    let (digits, _) = rest.split_once(after)?;
    digits.trim().parse().ok()
}

/// Read the file's bytes; `None` for one that is not the loader's.
#[must_use]
pub fn parse(bytes: &[u8]) -> Option<Kept<'_>> {
    let text = core::str::from_utf8(bytes).ok().or_else(|| {
        // A line cut in a multi-byte character, at the ring's wrap: what
        // reads is kept.
        core::str::from_utf8(bytes.get(..core::str::from_utf8(bytes).err()?.valid_up_to())?).ok()
    })?;
    let mut all = text.split('\n');
    let mut head = Vec::new();
    let first = all.next()?;
    let sequence = number_between(first, ": boot ", "'s kernel log, ")?;
    let written = number_between(first, "'s kernel log, ", " bytes, ")?;
    let kept = number_between(first, " bytes, ", " kept")?;
    head.push(first);
    let mut rest: Vec<&str> = Vec::new();
    for line in all.by_ref() {
        if line.starts_with("  pstore   ") && rest.is_empty() && head.len() < 8 {
            head.push(line);
        } else {
            rest.push(line);
            break;
        }
    }
    rest.extend(all);
    // The text's last line ends with its newline, so the split leaves an
    // empty piece after it; a last line with none is whole too.
    if rest.last() == Some(&"") {
        let _ = rest.pop();
    }
    let cut = written > kept;
    if cut && !rest.is_empty() {
        let _ = rest.remove(0);
    }
    Some(Kept {
        sequence,
        cut,
        lines: rest
            .into_iter()
            .map(|line| line.trim_end_matches('\r'))
            .collect(),
        head,
    })
}

/// The pstore boot number a kernel record's `message` gives its own boot,
/// if it is the kernel's `pstore   mirroring the log to ...` line.
#[must_use]
pub fn mirrored_boot(message: &str) -> Option<u64> {
    let rest = message
        .trim_start()
        .strip_prefix("pstore   mirroring the log to ")?;
    number_between(rest, ", boot ", ";")
}

/// The lines of `kept` after the last `RUN` of `stored` -- the messages of
/// the boot's kernel log records lode holds, in order -- found together,
/// searching from the end.
/// All of them when lode stored none; `None` when it stored some and they are
/// not there, so nothing is imported twice.
#[must_use]
pub fn unstored<'a>(kept: &Kept<'a>, stored: &[&str]) -> Option<Vec<&'a str>> {
    if stored.is_empty() {
        return Some(kept.lines.clone());
    }
    let run = stored.get(stored.len().saturating_sub(RUN)..)?;
    let lines = &kept.lines;
    let end = (run.len()..=lines.len()).rev().find(|&end| {
        lines
            .get(end - run.len()..end)
            .is_some_and(|window| window.iter().zip(run).all(|(line, want)| same(line, want)))
    })?;
    Some(lines.get(end..).map(<[&str]>::to_vec).unwrap_or_default())
}

/// Whether the region's `line` is the record lode stored as `message`: the
/// same text, or the text with what lode took off as the record's
/// identifier or level before it (`ingest::kernel`).
fn same(line: &str, message: &str) -> bool {
    !message.is_empty() && line.ends_with(message)
}

/// The records `lines` become in the boot they are of: `_TRANSPORT=pstore`,
/// the kernel's identifier, numbered after the boot's last stored kernel line
/// (`PSTORE_LINE`), all at `at`, the boot's last record's time.
#[must_use]
pub fn records(lines: &[&str], sequence: u64, at: Now) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let mut record = Record::new(6, at.realtime_us, at.monotonic_us);
            record.push("MESSAGE", line.as_bytes());
            record.push("SYSLOG_IDENTIFIER", b"kernel");
            record.push("_TRANSPORT", b"pstore");
            record.push("PSTORE_BOOT", alloc::format!("{sequence}").as_bytes());
            record.push("PSTORE_LINE", alloc::format!("{}", index + 1).as_bytes());
            record
        })
        .collect()
}

/// The import's own record, in this boot: how many lines of which boot.
#[must_use]
pub fn said(count: usize, sequence: u64, boot: &str, now: Now) -> Record {
    let mut record = ingest::own(
        5,
        MESSAGE_ID_IMPORTED,
        &alloc::format!(
            "the end of boot {boot} imported from the loader's {PATH}: {count} kernel line(s) lode \
             had not stored (pstore boot {sequence})"
        ),
        now,
    );
    record.push("PSTORE_IMPORTED", alloc::format!("{count}").as_bytes());
    record.push("PSTORE_BOOT", alloc::format!("{sequence}").as_bytes());
    record
}

/// Why nothing was imported, for one record.
#[must_use]
pub fn not_imported(why: &str, now: Now) -> Record {
    ingest::own(
        5,
        MESSAGE_ID_IMPORTED,
        &alloc::format!("the loader's {PATH} was not imported: {why}"),
        now,
    )
}

/// `String` of `bytes` for a reason line.
#[must_use]
pub fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
