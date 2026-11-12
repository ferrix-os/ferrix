//! The boot splash on a desktop a person uses (`docs/SPLASH.md`): the word
//! that has the loader paint it, and `/bin/splash` with the units that keep
//! it up until the compositor starts.
//!
//! `run-compositor`'s desktop and the release image carry it. The gates'
//! boots do not: their images and their command lines stay as they were,
//! and the unit runs only where the command line has the word.

use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::{Error, Result};

/// The command-line word the loader and `splash.service` read.
pub(crate) const WORD: &str = "ferrix.splash";

/// Where the program goes.
const PROGRAM: &str = "bin/splash";

/// When `splash.service` runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum When {
    /// Where the command line has [`WORD`]: an image whose words are
    /// written somewhere else, as the N150 box's are by its deploy.
    Word,
    /// Always: an image whose command line is not the image's to write,
    /// as the Pixel 7's is the app's.
    Always,
}

/// The unit: started beside `devmgr`, before `/` is known, by init's
/// `early.target` -- most of a boot's wait is the drivers and the root
/// volume -- and with the first units where the kernel starts `devmgr`
/// itself; until shutdown or the compositor's `splash --quit`. The program
/// waits for card0 itself, so a machine with no screen holds nothing up.
const UNIT: &str = "\
# The boot splash on card0 (docs/SPLASH.md).
[Unit]
Description=The boot splash
@CONDITION@DefaultDependencies=no
Conflicts=shutdown.target
Before=shutdown.target

[Service]
ExecStart=/bin/splash
StandardOutput=console
StandardError=console
";

/// The target init starts beside `devmgr.service` when the image has it
/// (`docs/INIT.md` §7.3).
const EARLY: &str = "\
# What starts before / is known (docs/INIT.md §7.3).
[Unit]
Description=Before the root volume
DefaultDependencies=no
";

/// What `hyprix.service` gains: the splash leaves the card before the
/// compositor opens it, since the compositor is master only if nobody is
/// (docs/DISPLAY.md §2.5). With no splash running it returns at once.
const DROP_IN: &str = "\
# The splash gives up card0 first (docs/SPLASH.md).
[Service]
ExecStartPre=-/bin/splash --quit
";

/// `defaults`, a `DEFAULTS.TXT`, with the splash's word added.
pub(crate) fn with_word(defaults: &str) -> String {
    format!("{} {WORD}\n", defaults.trim_end())
}

/// [`UNIT`]'s text for `when`.
fn unit(when: When) -> String {
    UNIT.replace(
        "@CONDITION@",
        match when {
            When::Word => "ConditionKernelCommandLine=ferrix.splash\n",
            When::Always => "",
        },
    )
}

/// What a desktop carries for the splash on `arch`, its unit running `when`.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn carried(arch: Arch, when: When) -> Result<Vec<File>> {
    let program = crate::compositor::splash_program(arch)?;
    let bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    let text = |path: &str, text: &str| File {
        path: path.to_owned(),
        mode: 0o644,
        content: Content::Bytes(text.as_bytes().to_vec()),
    };
    let wanted_by = |target: &str| File {
        path: format!("etc/ferrix/units/{target}.wants/splash.service"),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/splash.service".to_owned()),
    };
    Ok(vec![
        File {
            path: PROGRAM.to_owned(),
            mode: 0o755,
            content: Content::Bytes(bytes),
        },
        text("etc/ferrix/units/splash.service", &unit(when)),
        text("etc/ferrix/units/early.target", EARLY),
        wanted_by("early.target"),
        wanted_by("sysinit.target"),
        text("etc/ferrix/units/hyprix.service.d/splash.conf", DROP_IN),
    ])
}

#[cfg(test)]
mod tests {
    use super::{DROP_IN, WORD, When, unit, with_word};

    #[test]
    fn the_word_joins_the_defaults_on_their_one_line() {
        assert_eq!(
            with_word("ferrix.checks=skip\n"),
            "ferrix.checks=skip ferrix.splash\n"
        );
        assert_eq!(with_word("a b"), "a b ferrix.splash\n");
    }

    #[test]
    fn the_unit_runs_only_under_the_word_and_the_compositor_asks_it_to_leave() {
        assert!(unit(When::Word).contains(&format!("ConditionKernelCommandLine={WORD}\n")));
        assert!(!unit(When::Always).contains("Condition"));
        assert!(
            unit(When::Always)
                .contains("[Unit]\nDescription=The boot splash\nDefaultDependencies=no\n")
        );
        assert!(DROP_IN.contains("ExecStartPre=-/bin/splash --quit\n"));
    }
}
