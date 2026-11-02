//! Crash screenshots binned by program (`docs/LOGGING.md` §19): a program
//! in a crash loop gets one picture each [`SAME_PROGRAM_DEFAULT_US`], not
//! one per crash, so the loop does not also spend the desktop's and lode's
//! time on screenshots (Steam's browser on the N150, 2026-10-30: a renderer
//! crashed every 3-5 s, and each crash cost a full-screen readback, a blob
//! and its upload).
//!
//! `capture.conf`'s `SameProgramEvery=` sets the span (`lode reload` reads
//! it again); `0` takes a picture for every crash, as before.

use alloc::string::String;
use alloc::vec::Vec;

use crate::record::Record;

/// One picture per program each 10 min unless `capture.conf` says
/// otherwise.
pub const SAME_PROGRAM_DEFAULT_US: u64 = 10 * 60 * 1_000_000;

/// Programs remembered at once; the one longest unseen goes first.
const PROGRAMS_MAX: usize = 256;

/// What a record's crash is binned by: the path of the program that
/// crashed, else its name; for a record that names no process (a driver
/// devmgr says died, an error dialog) who said it and what, digits taken
/// out, so the same line about another pid or instance lands in one bin.
#[must_use]
pub fn bin_of(record: &Record) -> String {
    for field in ["CRASH_PROGRAM", "CRASH_EXE", "CRASH_COMM"] {
        if let Some(value) = record.get_str(field)
            && !value.is_empty()
        {
            return String::from(value);
        }
    }
    let who = record
        .get_str("FERRIX_SUBSYSTEM")
        .or_else(|| record.get_str("SYSLOG_IDENTIFIER"))
        .unwrap_or("?");
    let said: String = String::from_utf8_lossy(record.message())
        .chars()
        .filter(|c| !c.is_ascii_digit())
        .take(160)
        .collect();
    alloc::format!("{who}: {said}")
}

/// A picture already taken for the bin, so this crash gets none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Earlier {
    /// The crash record whose picture stands for this one.
    pub seqnum: u64,
    /// How long before, microseconds.
    pub ago_us: u64,
}

/// The bins: each program's last picture.
#[derive(Debug, Clone)]
pub struct Bins {
    /// The span; 0 lets every crash through.
    every_us: u64,
    /// Bin, when its picture was asked for (monotonic microseconds), and
    /// the crash record it was for.
    taken: Vec<(String, u64, u64)>,
}

impl Bins {
    /// Bins `every_us` wide; 0 for a picture per crash.
    #[must_use]
    pub fn new(every_us: u64) -> Bins {
        Bins {
            every_us,
            taken: Vec::new(),
        }
    }

    /// The span, for `lode reload` to say.
    #[must_use]
    pub fn every_us(&self) -> u64 {
        self.every_us
    }

    /// A new span (`lode reload`); what was taken is kept.
    pub fn set_every_us(&mut self, every_us: u64) {
        self.every_us = every_us;
    }

    /// Whether a crash in `bin` at `now_us` may have a picture: `Err` with
    /// the picture that stands for it when one was asked for within the
    /// span.
    ///
    /// # Errors
    ///
    /// [`Earlier`]: the bin's picture is younger than the span.
    pub fn check(&self, bin: &str, now_us: u64) -> Result<(), Earlier> {
        if self.every_us == 0 {
            return Ok(());
        }
        match self.taken.iter().find(|(name, _, _)| name == bin) {
            Some(&(_, at, seqnum)) if now_us.saturating_sub(at) < self.every_us => Err(Earlier {
                seqnum,
                ago_us: now_us.saturating_sub(at),
            }),
            _ => Ok(()),
        }
    }

    /// A picture was asked for, for crash record `seqnum` in `bin`.
    pub fn took(&mut self, bin: &str, now_us: u64, seqnum: u64) {
        if let Some(at) = self.taken.iter().position(|(name, _, _)| name == bin) {
            let _ = self.taken.remove(at);
        } else if self.taken.len() >= PROGRAMS_MAX
            && let Some(oldest) = self
                .taken
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, at, _))| *at)
                .map(|(index, _)| index)
        {
            let _ = self.taken.remove(oldest);
        }
        self.taken.push((String::from(bin), now_us, seqnum));
    }
}

/// `SameProgramEvery=`'s value: a duration (`10min`, `600`, `90s`), or
/// `0`, `always` or `every` for a picture per crash.
#[must_use]
pub fn same_program_every(value: &str) -> Option<u64> {
    match value.trim() {
        "always" | "every" | "every-crash" => Some(0),
        text => crate::limit::duration_us(text),
    }
}
