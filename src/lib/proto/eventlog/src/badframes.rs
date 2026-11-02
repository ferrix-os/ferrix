//! The bad-frame switch (`docs/MACHINE-CHECK-RESTART.md` §10): what a
//! `bad_frames = choice` loader keeps out across a restart, read from
//! `ferrix.badframes`, and which of the frames it kept out first it then
//! releases. A `bad_frames = persist` loader calls nothing here: its mode is
//! [`Mode::Persist`], fixed by its build.

/// The bad-frame mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Kept out across a warm reset (the pstore table) and a power cycle
    /// (the event log's set): 8a and 8b.
    Persist,
    /// Kept out across a warm reset alone (8a): nothing is written to the
    /// event log's set, and a frame kept out from it alone is released.
    Reset,
    /// Kept out across neither: every frame kept out is released.
    Off,
}

impl Mode {
    /// Its word on the command line.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Mode::Persist => "persist",
            Mode::Reset => "reset",
            Mode::Off => "off",
        }
    }
}

/// What [`choice`] read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice<'a> {
    /// No word: [`Mode::Persist`].
    Absent,
    /// A word understood.
    Chosen(Mode),
    /// Any other value: [`Mode::Persist`], the safe side, said.
    NotUnderstood(&'a str),
}

impl Choice<'_> {
    /// The mode it gives.
    #[must_use]
    pub const fn mode(self) -> Mode {
        match self {
            Choice::Chosen(mode) => mode,
            Choice::Absent | Choice::NotUnderstood(_) => Mode::Persist,
        }
    }
}

/// The option [`choice`] reads.
pub const OPTION: &str = "ferrix.badframes";

/// Read [`OPTION`] from `cmdline`, as `pstore::switch` reads its option.
#[must_use]
pub fn choice(cmdline: &str) -> Choice<'_> {
    match ferrix_bootinfo::option_in(cmdline, OPTION) {
        None => Choice::Absent,
        Some("persist") => Choice::Chosen(Mode::Persist),
        Some("reset") => Choice::Chosen(Mode::Reset),
        Some("off") => Choice::Chosen(Mode::Off),
        Some(other) => Choice::NotUnderstood(other),
    }
}

/// Whether, under `mode`, a frame the loader kept out is released after
/// the pstore region is taken (§10.3): under `reset` every frame the banks
/// and the table did not name (`named`) -- those the event log's set alone
/// kept out -- under `off` every frame, under `persist` none.
#[must_use]
pub fn released(mode: Mode, frame: u32, named: &[u32]) -> bool {
    match mode {
        Mode::Persist => false,
        Mode::Reset => !named.contains(&frame),
        Mode::Off => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{Choice, Mode, choice, released};

    /// Verifies the word's parse: absent and not understood are persist.
    #[test]
    fn the_word_is_read_and_anything_else_is_persist() {
        assert_eq!(choice("console=ttyS0"), Choice::Absent);
        assert_eq!(
            choice("ferrix.badframes=reset"),
            Choice::Chosen(Mode::Reset)
        );
        assert_eq!(
            choice("a ferrix.badframes=off b"),
            Choice::Chosen(Mode::Off)
        );
        assert_eq!(
            choice("ferrix.badframes=persist"),
            Choice::Chosen(Mode::Persist)
        );
        assert_eq!(choice("ferrix.badframes=Off"), Choice::NotUnderstood("Off"));
        assert_eq!(choice("ferrix.badframes=Off").mode(), Mode::Persist);
        assert_eq!(Choice::Absent.mode(), Mode::Persist);
    }

    /// Verifies the release set (ledger 5579 (4)): under reset a log frame
    /// the table or the banks also name stays out.
    #[test]
    fn reset_releases_the_log_alone_and_off_releases_all() {
        let named = [0x10, 0x20];
        for frame in [0x10, 0x20, 0x30] {
            assert!(!released(Mode::Persist, frame, &named), "{frame:#x}");
            assert!(released(Mode::Off, frame, &named), "{frame:#x}");
        }
        assert!(
            !released(Mode::Reset, 0x10, &named),
            "a table frame stays out"
        );
        assert!(
            !released(Mode::Reset, 0x20, &named),
            "a bank frame stays out"
        );
        assert!(
            released(Mode::Reset, 0x30, &named),
            "a log frame alone is released"
        );
    }
}
