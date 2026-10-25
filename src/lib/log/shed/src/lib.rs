//! Under load, the least severe goes first (`docs/LOGGING.md` §7.5, the
//! customer's rule of 2026-10-24).
//!
//! A [`Shed`] stands in front of one sink for one source -- a unit's lines
//! to the console, `lode cat`'s copy to its standard output, its queue to
//! lode -- and decides line by line whether it goes on. Over a window of
//! [`WINDOW_US`], a line of severity `s` is kept while the source's lines
//! kept so far in the window number fewer than the cap of `s`, and the caps
//! rise with severity ([`Caps`]): as a source writes faster, its debug lines
//! are shed first, then info, then notice, then warnings; errors and worse
//! only past a cap no ordinary source reaches. A source's lines count only
//! against its own caps, so a flooding program loses its debug lines before
//! a quiet program loses anything. This is journald's `RateLimitBurst=` per
//! source (`journal-ratelimit.c`), with the severity order of Android logd's
//! prune-by-priority (`LogBuffer::prune`, which removes the chattiest, least
//! important entries first) applied as the lines arrive.
//!
//! Nothing is shed silently: [`Shed::said`] gives, once a window that shed
//! anything is over, how many of each severity, to be written as one line at
//! the severity of the most severe line shed.

#![no_std]

/// The window caps count over, in microseconds: one second.
pub const WINDOW_US: u64 = 1_000_000;

/// The most lines a window keeps, by severity: a line of severity `s`
/// (syslog's 0 emerg .. 7 debug) is kept while fewer than `caps[s]` lines
/// were kept in the window. Each cap is at least the one after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps(pub [u32; 8]);

impl Caps {
    /// What a console takes from one source in a second: lines of every
    /// severity up to 100, of info or worse up to 1,000, warnings or worse
    /// up to 2,000, errors or worse up to 10,000. A console is printed as it
    /// comes, synchronously on a framebuffer console, so a debug flood is
    /// held to what a person reads, while a program's ordinary output -- a
    /// test's results, which gates read off the serial port -- passes.
    pub const CONSOLE: Caps = Caps([10_000, 10_000, 10_000, 10_000, 2_000, 1_000, 1_000, 100]);

    /// What a queue to lode takes from one source when lode is behind: lode
    /// keeps everything it takes, so far more, in the same order.
    pub const QUEUE: Caps = Caps([
        1 << 20,
        1 << 20,
        1 << 20,
        1 << 20,
        100_000,
        50_000,
        20_000,
        5_000,
    ]);
}

/// What one window shed, to be said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Said {
    /// Lines shed, by severity.
    pub shed: [u64; 8],
    /// The window's length, in microseconds.
    pub span_us: u64,
}

impl Said {
    /// The most severe severity shed: the level to say it at.
    #[must_use]
    pub fn level(&self) -> u8 {
        self.shed
            .iter()
            .position(|count| *count > 0)
            .and_then(|level| u8::try_from(level).ok())
            .unwrap_or(7)
    }

    /// How many lines in all.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.shed.iter().sum()
    }
}

/// One source's shed in front of one sink.
#[derive(Debug, Clone)]
pub struct Shed {
    /// Its caps.
    caps: Caps,
    /// When the window began.
    begun_us: u64,
    /// Lines kept in it.
    kept: u32,
    /// Lines shed in it, by severity.
    shed: [u64; 8],
    /// Shed in windows not yet said.
    owed: Option<Said>,
}

/// The severity a line of unknown level is taken at: info.
pub const UNKNOWN: u8 = 6;

impl Shed {
    /// A shed with `caps`, its first window beginning at `now_us`.
    #[must_use]
    pub const fn new(caps: Caps, now_us: u64) -> Shed {
        Shed {
            caps,
            begun_us: now_us,
            kept: 0,
            shed: [0; 8],
            owed: None,
        }
    }

    /// Whether a line of `severity` (0-7; anything else as [`UNKNOWN`])
    /// goes on at `now_us`.
    pub fn admit(&mut self, severity: u8, now_us: u64) -> bool {
        self.roll(now_us);
        let severity = if severity > 7 { UNKNOWN } else { severity };
        let cap = self.caps.0.get(usize::from(severity)).copied().unwrap_or(0);
        if self.kept < cap {
            self.kept += 1;
            true
        } else {
            if let Some(count) = self.shed.get_mut(usize::from(severity)) {
                *count += 1;
            }
            false
        }
    }

    /// End the window if `now_us` is past it, keeping what it shed to be
    /// said.
    fn roll(&mut self, now_us: u64) {
        if now_us.saturating_sub(self.begun_us) >= WINDOW_US {
            self.close(now_us);
        }
    }

    /// End the window at `now_us`, keeping what it shed to be said.
    fn close(&mut self, now_us: u64) {
        if self.shed.iter().any(|count| *count > 0) {
            let span_us = now_us.saturating_sub(self.begun_us);
            let owed = self.owed.get_or_insert(Said {
                shed: [0; 8],
                span_us: 0,
            });
            for (all, now) in owed.shed.iter_mut().zip(self.shed) {
                *all += now;
            }
            owed.span_us += span_us;
        }
        self.begun_us = now_us;
        self.kept = 0;
        self.shed = [0; 8];
    }

    /// Whether it has shed lines not said yet: its window, or one before.
    #[must_use]
    pub fn pending(&self) -> bool {
        self.owed.is_some() || self.shed.iter().any(|count| *count > 0)
    }

    /// What windows over at `now_us` shed and is not said yet: the caller
    /// says it as one line, at [`Said::level`].
    pub fn said(&mut self, now_us: u64) -> Option<Said> {
        self.roll(now_us);
        self.owed.take()
    }

    /// Everything shed and not said, the current window's too: at an end.
    pub fn last(&mut self, now_us: u64) -> Option<Said> {
        self.close(now_us);
        self.owed.take()
    }
}

/// The level a line says it is, if it says: a `<N>` prefix (syslog's,
/// as journald's stdout streams read it), else the first of Rust's and
/// most loggers' level words in its first 80 bytes -- `ERROR`, `WARN`,
/// `INFO`, `DEBUG`, `TRACE`, as whole words, in upper case.
#[must_use]
pub fn level_of(line: &[u8]) -> Option<u8> {
    if let [b'<', digit @ b'0'..=b'7', b'>', ..] = line {
        return Some(digit - b'0');
    }
    let head = line.get(..line.len().min(80)).unwrap_or_default();
    let mut words = head.split(|byte| !byte.is_ascii_alphabetic());
    words.find_map(|word| match word {
        b"ERROR" | b"FATAL" | b"CRIT" => Some(3),
        b"WARN" | b"WARNING" => Some(4),
        b"NOTICE" => Some(5),
        b"INFO" => Some(6),
        b"DEBUG" | b"TRACE" => Some(7),
        _ => None,
    })
}

/// The words of the line that says `said` for `source`: `N lines dropped
/// (debug: a, info: b) from SOURCE in T s`, into `out`.
pub fn describe(said: &Said, source: &str, out: &mut impl core::fmt::Write) -> core::fmt::Result {
    const NAMES: [&str; 8] = [
        "emerg", "alert", "crit", "err", "warning", "notice", "info", "debug",
    ];
    write!(out, "{} lines dropped (", said.total())?;
    let mut first = true;
    for (level, count) in said.shed.iter().enumerate().rev() {
        if *count == 0 {
            continue;
        }
        if !first {
            out.write_str(", ")?;
        }
        first = false;
        write!(out, "{}: {count}", NAMES.get(level).copied().unwrap_or("?"))?;
    }
    write!(
        out,
        ") from {source} in {}.{} s",
        said.span_us / 1_000_000,
        said.span_us / 100_000 % 10
    )
}

#[cfg(test)]
mod tests;
