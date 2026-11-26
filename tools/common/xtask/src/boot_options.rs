//! A row's boot options against its profile's permitted set (ledger 7013 LV.5,
//! 10312 M1-M6).
//!
//! Each profile file names, of the registry's boot options, the values it permits
//! (`tools/common/data/profiles/<profile>.toml`, `[boot_options]`). A row built
//! under `--kernel-profile P` writes an image whose command line the kernel reads:
//! `FERRIX/CMDLINE.TXT`, and `FERRIX/DEFAULTS.TXT` after it. Every word of both is
//! judged against P's set: an option P does not list, or a value its list does not
//! hold, is outside it (M1). At level `none` the image is written and one line names
//! what is outside, since such a row is not P's row and counts toward no `built`, no
//! sweep and no flip (M2, M4); `fast` and `max` above level `none` refuse the image
//! by name. `present`, at level `present` today, prints too: its gates' images carry
//! options outside its set (ARMv7-A's `ferrix.unisolated`, a skipped-checks boot),
//! and whether they refuse is the consultant's question, not this landing's.
//!
//! An option a test needs and its profile does not permit may be excused for that
//! test alone by [`EXCEPTIONS`], with its reason; `ferrix.unisolated` never is under
//! `fast` or `max` (M2).
//!
//! What this does not judge: an ad-hoc build (`--set`, profile `none`), which is no
//! profile's; and a command line given to QEMU with `-append` or in a device tree's
//! `/chosen/bootargs`, which no row under a profile writes today.

use std::sync::OnceLock;

use crate::flags::Profile;
use crate::{Error, Result};

/// The command this run of xtask carries out, set once at dispatch, for
/// [`EXCEPTIONS`].
static COMMAND: OnceLock<String> = OnceLock::new();

/// Name the command this run carries out.
pub(crate) fn set_command(command: &str) {
    let _ = COMMAND.set(command.to_owned());
}

/// Options a test may carry outside its profile's set, each with its reason:
/// `(test, option, why)`. Empty: no row has a reason accepted yet.
const EXCEPTIONS: &[(&str, &str, &str)] = &[];

/// The words no exception may excuse under `fast` or `max`.
const NEVER_EXCUSED: &[&str] = &["ferrix.unisolated"];

/// The words of `line` outside `permitted`: an option it does not list, or a value
/// its list does not hold (`*` holds any one word; a bare word is permitted only if
/// listed).
pub(crate) fn outside(permitted: &[(String, Vec<String>)], line: &str) -> Vec<String> {
    line.split_whitespace()
        .filter(|word| {
            let (name, value) = match word.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (*word, None),
            };
            let Some((_, values)) = permitted.iter().find(|(option, _)| option == name) else {
                return true;
            };
            match value {
                None => !values.iter().any(|allowed| allowed == "true"),
                Some(value) => !values
                    .iter()
                    .any(|allowed| allowed == "*" || allowed == value),
            }
        })
        .map(str::to_owned)
        .collect()
}

/// [`outside`] less what `test` is excused under `profile`.
fn not_excused(profile: &str, test: &str, words: Vec<String>) -> Vec<String> {
    words
        .into_iter()
        .filter(|word| {
            let name = word.split_once('=').map_or(word.as_str(), |(name, _)| name);
            let never = matches!(profile, "fast" | "max") && NEVER_EXCUSED.contains(&name);
            never
                || !EXCEPTIONS
                    .iter()
                    .any(|(excused, option, _)| *excused == test && *option == name)
        })
        .collect()
}

/// What to do with an image whose command line has `words` outside `profile`'s
/// set: `Ok(Some(line))` to say at level `none`, `Err` above it, `Ok(None)` when
/// nothing is outside.
pub(crate) fn verdict(profile: &Profile, words: &[String]) -> Result<Option<String>> {
    if words.is_empty() {
        return Ok(None);
    }
    let said = format!(
        "boot options outside {}'s permitted set: {}",
        profile.name,
        words.join(" ")
    );
    if profile.level == "none" || !matches!(profile.name.as_str(), "fast" | "max") {
        return Ok(Some(format!(
            "  {said}: this row is not {}'s row (ledger 10312 M2)",
            profile.name
        )));
    }
    Err(Error::new(format!(
        "{said}: an image above level none carries only what its profile permits"
    )))
}

/// Judge the image's command line, `cmdline` then `defaults`, against this run's
/// profile, if it has one.
///
/// # Errors
///
/// A word outside the set above level `none`, or the profile unreadable.
pub(crate) fn judge(cmdline: Option<&str>, defaults: Option<&str>) -> Result<()> {
    let resolved = crate::flags::this_run()?;
    if resolved.profile == "none" {
        return Ok(());
    }
    let profile = Profile::load(&resolved.profile)?;
    let line = format!("{} {}", cmdline.unwrap_or(""), defaults.unwrap_or(""));
    let test = COMMAND.get().map_or("", String::as_str);
    let words = not_excused(&profile.name, test, outside(&profile.boot_options, &line));
    if let Some(said) = verdict(&profile, &words)? {
        println!("{said}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(entries: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
        entries
            .iter()
            .map(|(option, values)| {
                (
                    (*option).to_owned(),
                    values.iter().map(|value| (*value).to_owned()).collect(),
                )
            })
            .collect()
    }

    fn profile(name: &str, level: &str) -> Profile {
        Profile {
            name: name.to_owned(),
            level: level.to_owned(),
            target_level: None,
            values: Vec::new(),
            exceptions: Vec::new(),
            image: Vec::new(),
            boot_options: Vec::new(),
        }
    }

    #[test]
    fn a_word_outside_the_set_is_named() {
        let permitted = set(&[
            ("ferrix.checks", &["run"]),
            ("ferrix.init", &["*"]),
            ("console", &["*"]),
        ]);
        assert!(
            outside(
                &permitted,
                "ferrix.checks=run ferrix.init=/sbin/init console=ttyS0"
            )
            .is_empty()
        );
        assert_eq!(
            outside(&permitted, "ferrix.checks=skip"),
            ["ferrix.checks=skip"]
        );
        assert_eq!(
            outside(&permitted, "ferrix.unisolated"),
            ["ferrix.unisolated"]
        );
        assert_eq!(outside(&permitted, "nokaslr ferrix.init=/x"), ["nokaslr"]);
        assert!(outside(&permitted, "").is_empty());
        let bare = set(&[("ferrix.fbcon", &["true"])]);
        assert!(outside(&bare, "ferrix.fbcon").is_empty());
        assert_eq!(outside(&bare, "ferrix.fbcon=1"), ["ferrix.fbcon=1"]);
    }

    #[test]
    fn unisolated_is_never_excused_under_fast_or_max() {
        let words = vec!["ferrix.unisolated".to_owned()];
        assert_eq!(
            not_excused("max", "test-driver-model", words.clone()),
            words
        );
        assert_eq!(
            not_excused("fast", "test-driver-model", words.clone()),
            words
        );
    }

    #[test]
    fn level_none_says_and_a_raised_level_refuses() {
        let words = vec!["ferrix.checks=skip".to_owned()];
        let said = verdict(&profile("max", "none"), &words).ok().flatten();
        assert!(said.is_some_and(|line| line.contains("not max's row")));
        assert!(verdict(&profile("max", "sc4"), &words).is_err());
        assert!(matches!(verdict(&profile("max", "sc4"), &[]), Ok(None)));
        assert!(matches!(
            verdict(&profile("present", "present"), &words),
            Ok(Some(_))
        ));
    }
}
