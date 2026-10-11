//! What the loader tries, in which order (`docs/AB-BOOT.md` §4), where it
//! may write, and the command-line words it puts first.

use core::fmt::{self, Write};

use crate::Slot;
use crate::gpt::Part;
use crate::record::{COPY_OFFSETS, Current, RECORD_LEN};

/// How a slot is being booted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A trial: started only after the record with one try fewer was
    /// written and read back.
    Trial,
    /// The good slot, or slot A or B on an uninitialised record.
    Good,
    /// The other slot, because the good one did not verify.
    Fallback,
}

impl Kind {
    /// The kind `ferrix.slot.boot=` names.
    #[must_use]
    pub fn from_word(word: &str) -> Option<Kind> {
        match word {
            "trial" => Some(Kind::Trial),
            "good" => Some(Kind::Good),
            "fallback" => Some(Kind::Fallback),
            _ => None,
        }
    }

    /// The word `ferrix.slot.boot=` says.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Kind::Trial => "trial",
            Kind::Good => "good",
            Kind::Fallback => "fallback",
        }
    }
}

/// One slot to try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// Which slot.
    pub slot: Slot,
    /// How.
    pub kind: Kind,
}

/// The attempts, in order; at most three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    attempts: [Option<Attempt>; 3],
}

impl Plan {
    /// The attempts in order.
    pub fn attempts(&self) -> impl Iterator<Item = Attempt> + '_ {
        self.attempts.iter().flatten().copied()
    }
}

/// The attempts for the current record.
///
/// * uninitialised: A, then B, both as good, and nothing is ever written;
/// * a trial with tries left: the trial, then the good slot. No fallback:
///   the good slot's other is the trial slot, and reaching it means the
///   trial did not verify or its try could not be recorded, so starting it
///   again without a recorded try is exactly the loop the record prevents;
/// * a trial that used its tries (trial set, tries 0): the good slot alone.
///   The trial slot failed; were the good one to stop verifying, starting
///   the failed trial on every reset would be worse than the ESP's rescue
///   files, which the loader turns to after the plan;
/// * no trial: the good slot, then the other as a fallback.
#[must_use]
pub fn plan(current: &Current) -> Plan {
    let good = |slot| Attempt {
        slot,
        kind: Kind::Good,
    };
    let attempts = match current {
        Current::Uninitialised => [Some(good(Slot::A)), Some(good(Slot::B)), None],
        Current::Valid { record, .. } => match record.trial {
            Some(trial) if record.tries > 0 => [
                Some(Attempt {
                    slot: trial,
                    kind: Kind::Trial,
                }),
                Some(good(record.good)),
                None,
            ],
            Some(_) => [Some(good(record.good)), None, None],
            None => [
                Some(good(record.good)),
                Some(Attempt {
                    slot: record.good.other(),
                    kind: Kind::Fallback,
                }),
                None,
            ],
        },
    };
    Plan { attempts }
}

/// The block a record copy is written to and read from: `copy`'s byte
/// offset in the control partition, in blocks of `block_size`, checked to
/// lie with the whole record inside the partition. The loader writes no
/// other block.
#[must_use]
pub fn record_block(control: &Part, copy: usize, block_size: u32) -> Option<u64> {
    let offset = *COPY_OFFSETS.get(copy)?;
    let block_size = u64::from(block_size);
    if block_size < RECORD_LEN as u64 || offset % block_size != 0 {
        return None;
    }
    let lba = control.first_lba.checked_add(offset / block_size)?;
    // The record and the rest of its block inside the partition.
    (lba <= control.last_lba).then_some(lba)
}

/// The words the loader puts before a slot's own command line.
///
/// First, because the kernel takes the first word with a key, so the
/// slot's command line cannot forge them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoaderWords {
    /// The slot booted.
    pub slot: Slot,
    /// How.
    pub kind: Kind,
    /// The sequence of the record acted on: the one written, for a trial.
    pub sequence: u64,
    /// The boot disk's GPT disk GUID, so `bootctl` writes only that disk.
    pub disk: crate::gpt::Guid,
}

impl fmt::Display for LoaderWords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ferrix.slot={} ferrix.slot.boot={} ferrix.slot.seq={} ferrix.slot.disk={}",
            self.slot.letter(),
            self.kind.word(),
            self.sequence,
            crate::gpt::GuidText(self.disk)
        )
    }
}

/// The loader's words read back from a kernel command line, as `bootctl`
/// reads `/proc/cmdline`: the inverse of [`LoaderWords`]' `Display`. For
/// each key the first word with it counts, as the kernel takes it, so a
/// slot's own command line after the loader's words cannot override them.
/// `None` unless all four are there and each is well formed: a boot that
/// did not come from a slot, or one whose words cannot be trusted.
#[must_use]
pub fn parse_words(cmdline: &str) -> Option<LoaderWords> {
    let first = |key: &str| {
        cmdline
            .split_ascii_whitespace()
            .find_map(|word| word.strip_prefix(key))
    };
    Some(LoaderWords {
        slot: Slot::from_letter(first("ferrix.slot=")?)?,
        kind: Kind::from_word(first("ferrix.slot.boot=")?)?,
        sequence: first("ferrix.slot.seq=")?.parse().ok()?,
        disk: crate::gpt::parse_guid(first("ferrix.slot.disk=")?)?,
    })
}

/// [`LoaderWords`] written into `out`; the number of bytes, or `None` when
/// they do not fit.
#[must_use]
pub fn write_words(words: &LoaderWords, out: &mut [u8]) -> Option<usize> {
    /// A `fmt::Write` over a byte slice that fails instead of truncating.
    struct Cursor<'a> {
        out: &'a mut [u8],
        used: usize,
    }
    impl Write for Cursor<'_> {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            let end = self.used.checked_add(text.len()).ok_or(fmt::Error)?;
            let slot = self.out.get_mut(self.used..end).ok_or(fmt::Error)?;
            slot.copy_from_slice(text.as_bytes());
            self.used = end;
            Ok(())
        }
    }
    let mut cursor = Cursor { out, used: 0 };
    write!(cursor, "{words}").ok()?;
    Some(cursor.used)
}

/// The whole command line of a slot boot written into `out`: the loader's
/// words, a space, and the slot's command line with surrounding whitespace
/// dropped. The number of bytes, or `None` when the slot's line is not
/// UTF-8 or the whole does not fit -- then the slot does not verify; the
/// loader never truncates (P5).
#[must_use]
pub fn compose(words: &LoaderWords, slot_line: &[u8], out: &mut [u8]) -> Option<usize> {
    let line = core::str::from_utf8(slot_line).ok()?.trim();
    let used = write_words(words, out)?;
    if line.is_empty() {
        return Some(used);
    }
    let end = used.checked_add(1)?.checked_add(line.len())?;
    let rest = out.get_mut(used..end)?;
    let (space, text) = rest.split_first_mut()?;
    *space = b' ';
    text.copy_from_slice(line.as_bytes());
    Some(end)
}
