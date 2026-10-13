//! The build flags: a kernel profile, and any `--set`, resolved into the
//! `cfg`s the kernel is compiled with (`docs/BUILD-FLAGS.md`).
//!
//! Everything comes from two kinds of file: the flag file,
//! `tools/common/data/build-flags.toml`, which declares every flag, value,
//! level and boot option, and the profile files in
//! `tools/common/data/profiles/`, each of which sets every flag. A build
//! names one profile (`--kernel-profile`, `present` when none is named) and
//! may change single flags with `--set flag=value`; a build that changed any
//! is ad hoc, `profile=none level=none`, and carries no claim (rule 5).
//!
//! **What this resolver is trusted with.** Little: it is a T2 tool, kept so
//! by rule 6. The kernel computes its own `build` boot line from the `cfg`s it
//! was compiled with, and the gate (`tools/common/check/check-boot-line.py`)
//! compares that line with the profile file, read by a parser of its own,
//! never with anything printed here. So a resolver that set the wrong value
//! makes a kernel the gate refuses, not a wrong claim.
//!
//! **How the flags reach the kernel, and only the kernel.** Rule 6 b reads
//! flags in the kernel crate alone, since a library built without a `cfg`
//! would take its other branch without a word. So the flags are not
//! rustflags, which reach every crate: xtask hands the resolved set to the
//! kernel's build script in `FERRIX_BUILD_FLAGS`, and the declared flags and
//! values in `FERRIX_FLAG_VALUES`; the script turns them into
//! `cargo::rustc-cfg=ferrix_<flag>="<value>"` and
//! `cargo::rustc-check-cfg` for the kernel crate alone. The workspace denies
//! `unexpected_cfgs`, so a `cfg` naming an undeclared flag or value fails the
//! kernel's build, and one in any other crate fails that crate's, which has
//! no `ferrix_` cfg declared at all. `mitigations=off` also changes codegen
//! for every crate (the static relocation model), which stays a target
//! rustflag ([`crate::cargo`]).

mod toml;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::paths;
use crate::task::Task;
use crate::{Error, Result};

pub(crate) use self::toml::Value;

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "flags",
    help_at: 195,
    help: &[
        "flags         The build flags --kernel-profile and --set resolve to, the cfgs the kernel is",
        "              compiled with, the build line it will print, and whether it builds today",
    ],
    run: |_, _| command(),
}];

/// The profile a build names when it names none: today's reference
/// configuration (BF1).
pub(crate) const DEFAULT_PROFILE: &str = "present";

/// Where the flag file is.
pub(crate) fn flag_file() -> PathBuf {
    paths::workspace_root().join("tools/common/data/build-flags.toml")
}

/// Where the profile files are.
pub(crate) fn profile_dir() -> PathBuf {
    paths::workspace_root().join("tools/common/data/profiles")
}

/// Whether a value's code is in the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Built {
    /// It is, and the value builds.
    Yes,
    /// It builds today's code of a stricter value in its place: only at
    /// level `none`.
    StandIn,
    /// Not yet: refused, naming the stage it lands at.
    No,
}

/// One value of a flag, as the flag file declares it.
#[derive(Debug, Clone)]
pub(crate) struct FlagValue {
    /// Its name.
    pub(crate) name: String,
    /// The levels it allows.
    pub(crate) allows: Vec<String>,
    /// Where its code is or will be.
    pub(crate) lands: String,
    /// Whether it builds.
    pub(crate) built: Built,
    /// For a stand-in, the value whose code it builds.
    pub(crate) stands_in: Option<String>,
    /// `flag=value`s it requires.
    pub(crate) requires: Vec<(String, String)>,
    /// `flag=value`s it conflicts with.
    pub(crate) conflicts: Vec<(String, String)>,
}

/// One flag.
#[derive(Debug, Clone)]
pub(crate) struct Flag {
    /// Its name: the `cfg` is `ferrix_<name>`.
    pub(crate) name: String,
    /// Its values, in the file's order.
    pub(crate) values: Vec<FlagValue>,
}

impl Flag {
    /// The value called `name`.
    pub(crate) fn value(&self, name: &str) -> Option<&FlagValue> {
        self.values.iter().find(|value| value.name == name)
    }
}

/// The flag file.
#[derive(Debug, Clone)]
pub(crate) struct FlagFile {
    /// The flags, in the order the boot line names them.
    pub(crate) flags: Vec<Flag>,
    /// The levels' names.
    pub(crate) levels: Vec<String>,
}

impl FlagFile {
    /// Read and check the flag file.
    pub(crate) fn load() -> Result<FlagFile> {
        let path = flag_file();
        let root = read_toml(&path)?;
        let levels: Vec<String> = root
            .get("levels")
            .map(|levels| {
                levels
                    .entries()
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default();
        if levels.is_empty() {
            return Err(Error::new(format!("{} declares no levels", path.display())));
        }
        let mut flags = Vec::new();
        for (name, table) in root.get("flags").map(Value::entries).unwrap_or_default() {
            flags.push(flag(name, table, &levels)?);
        }
        if flags.is_empty() {
            return Err(Error::new(format!("{} declares no flags", path.display())));
        }
        Ok(FlagFile { flags, levels })
    }

    /// The flag called `name`.
    pub(crate) fn flag(&self, name: &str) -> Option<&Flag> {
        self.flags.iter().find(|flag| flag.name == name)
    }
}

/// A flag's table from the flag file.
fn flag(name: &str, table: &Value, levels: &[String]) -> Result<Flag> {
    let what = |why: &str| Error::new(format!("build-flags.toml: flag {name}: {why}"));
    check_word(name).map_err(|why| what(&why))?;
    let mut values = Vec::new();
    for (value, entry) in table.get("values").map(Value::entries).unwrap_or_default() {
        check_word(value).map_err(|why| what(&why))?;
        let strings = |key: &str| entry.get(key).and_then(Value::strings).unwrap_or_default();
        let allows = strings("allows");
        if let Some(unknown) = allows.iter().find(|level| !levels.contains(level)) {
            return Err(what(&format!(
                "{value} allows an undeclared level `{unknown}`"
            )));
        }
        let built = match entry.get("built").and_then(Value::as_str) {
            Some("yes") => Built::Yes,
            Some("stand-in") => Built::StandIn,
            Some("no") => Built::No,
            _ => return Err(what(&format!("{value}: `built` is yes, stand-in or no"))),
        };
        let pairs = |key: &str| -> Result<Vec<(String, String)>> {
            strings(key).iter().map(|pair| assignment(pair)).collect()
        };
        values.push(FlagValue {
            name: value.clone(),
            allows,
            lands: entry
                .get("lands")
                .and_then(Value::as_str)
                .ok_or_else(|| what(&format!("{value} names no `lands`")))?
                .to_owned(),
            built,
            stands_in: entry
                .get("stands_in")
                .and_then(Value::as_str)
                .map(str::to_owned),
            requires: pairs("requires")?,
            conflicts: pairs("conflicts")?,
        });
    }
    if values.len() < 2 {
        return Err(what("a flag has two values at least"));
    }
    Ok(Flag {
        name: name.to_owned(),
        values,
    })
}

/// `flag=value`, split.
pub(crate) fn assignment(text: &str) -> Result<(String, String)> {
    let (flag, value) = text
        .split_once('=')
        .ok_or_else(|| Error::new(format!("`{text}` is not flag=value")))?;
    Ok((flag.trim().to_owned(), value.trim().to_owned()))
}

/// A flag's or value's name: what a `cfg` and the boot line can carry.
fn check_word(word: &str) -> std::result::Result<(), String> {
    if !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        Ok(())
    } else {
        Err(format!("`{word}` is not lower case, digits and `_`"))
    }
}

/// Read a TOML file.
fn read_toml(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    toml::parse(&path.display().to_string(), &text)
}

/// The profiles there are, by name, sorted.
pub(crate) fn profile_names() -> Result<Vec<String>> {
    let dir = profile_dir();
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)
        .map_err(|error| Error::new(format!("reading {}: {error}", dir.display())))?
    {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
            && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
        {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// A profile file.
#[derive(Debug, Clone)]
pub(crate) struct Profile {
    /// Its name.
    pub(crate) name: String,
    /// Its level.
    pub(crate) level: String,
    /// Its flags' values, as the file lists them.
    pub(crate) values: Vec<(String, String)>,
    /// The `flag=value`s it uses below its level by a dated exception.
    pub(crate) exceptions: Vec<(String, String)>,
}

impl Profile {
    /// Read the profile called `name`.
    pub(crate) fn load(name: &str) -> Result<Profile> {
        check_word(name).map_err(|why| Error::new(format!("--kernel-profile {name}: {why}")))?;
        let path = profile_dir().join(format!("{name}.toml"));
        if !path.is_file() {
            return Err(Error::new(format!(
                "no kernel profile `{name}`: the profiles are {}",
                profile_names()?.join(", ")
            )));
        }
        let root = read_toml(&path)?;
        let what = |why: &str| Error::new(format!("{}: {why}", path.display()));
        if root.get("profile").and_then(Value::as_str) != Some(name) {
            return Err(what("its `profile` is not its file's name"));
        }
        let level = root
            .get("level")
            .and_then(Value::as_str)
            .ok_or_else(|| what("it names no level"))?
            .to_owned();
        let mut values = Vec::new();
        for (flag, value) in root.get("flags").map(Value::entries).unwrap_or_default() {
            let value = value
                .as_str()
                .ok_or_else(|| what(&format!("flag {flag} is not a string")))?;
            values.push((flag.clone(), value.to_owned()));
        }
        let mut exceptions = Vec::new();
        if let Some(Value::Array(items)) = root.get("exceptions") {
            for item in items {
                let field = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
                match (field("flag"), field("value")) {
                    (Some(flag), Some(value)) => exceptions.push((flag, value)),
                    _ => return Err(what("an exception names no flag or value")),
                }
            }
        }
        Ok(Profile {
            name: name.to_owned(),
            level,
            values,
            exceptions,
        })
    }
}

/// A build's flags, resolved.
#[derive(Debug, Clone)]
pub(crate) struct Resolved {
    /// The profile named, whether or not the build still is it.
    pub(crate) base: String,
    /// `profile=` on the boot line: the profile, or `none` for an ad-hoc
    /// build.
    pub(crate) profile: String,
    /// `level=` on the boot line.
    pub(crate) level: String,
    /// Every flag's value, in the flag file's order.
    pub(crate) values: Vec<(String, String)>,
    /// The `--set`s that changed a value.
    pub(crate) changed: Vec<(String, String)>,
    /// Every flag's declared values, for `--check-cfg`.
    declared: Vec<(String, Vec<String>)>,
}

impl Resolved {
    /// The value of `flag`.
    pub(crate) fn value(&self, flag: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(name, _)| name == flag)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the kernel is built without its side-channel defences.
    pub(crate) fn mitigations_off(&self) -> bool {
        self.value("mitigations") == Some("off")
    }

    /// `FERRIX_BUILD_FLAGS`: the `cfg`s, as `name=value` words.
    pub(crate) fn cfg_words(&self) -> String {
        let mut words = vec![
            format!("profile={}", self.profile),
            format!("level={}", self.level),
        ];
        words.extend(
            self.values
                .iter()
                .map(|(flag, value)| format!("{flag}={value}")),
        );
        words.join(" ")
    }

    /// `FERRIX_FLAG_VALUES`: every declared flag and its values, as
    /// `name=a,b` words, for the kernel build script's `--check-cfg`.
    pub(crate) fn declared_words(&self) -> String {
        self.declared
            .iter()
            .map(|(flag, values)| format!("{flag}={}", values.join(",")))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The `build` line the kernel will print, as this resolver sees it:
    /// for `cargo xtask flags` to show. The gate never reads it.
    pub(crate) fn boot_line(&self) -> String {
        let claim = if self.profile == "none" {
            " ; no claim"
        } else {
            ""
        };
        format!("build    {}{claim}", self.cfg_words())
    }
}

/// Resolve `profile` (or [`DEFAULT_PROFILE`]) with `sets` applied, and check
/// that the result builds.
///
/// # Errors
///
/// A file that does not parse; a profile that does not set every declared
/// flag, or sets one undeclared or to an undeclared value; a `--set` naming
/// either; a value that requires or conflicts with another the build has; a
/// value whose level the profile's level does not allow, outside the
/// profile's dated exceptions; and a value that does not build yet, naming
/// the stage it lands at.
pub(crate) fn resolve(profile: Option<&str>, sets: &[(String, String)]) -> Result<Resolved> {
    let resolved = resolve_unbuilt(profile, sets)?;
    buildable(&resolved)?;
    Ok(resolved)
}

/// [`resolve`] without the last check: whether the result builds today.
pub(crate) fn resolve_unbuilt(
    profile: Option<&str>,
    sets: &[(String, String)],
) -> Result<Resolved> {
    let file = FlagFile::load()?;
    let profile = Profile::load(profile.unwrap_or(DEFAULT_PROFILE))?;
    let mut names = profile_names()?;
    names.push("none".to_owned());
    let mut levels = file.levels.clone();
    if !levels.iter().any(|level| level == "none") {
        levels.push("none".to_owned());
    }
    if !levels.contains(&profile.level) {
        return Err(Error::new(format!(
            "profile {}: level `{}` is not declared",
            profile.name, profile.level
        )));
    }
    let mut values = Vec::new();
    for flag in &file.flags {
        let value = profile
            .values
            .iter()
            .find(|(name, _)| *name == flag.name)
            .map(|(_, value)| value.clone())
            .ok_or_else(|| {
                Error::new(format!(
                    "profile {} does not set the flag {} (rule 4: every flag, explicitly)",
                    profile.name, flag.name
                ))
            })?;
        values.push((flag.name.clone(), value));
    }
    if let Some((stray, _)) = profile
        .values
        .iter()
        .find(|(name, _)| file.flag(name).is_none())
    {
        return Err(Error::new(format!(
            "profile {} sets `{stray}`, which build-flags.toml does not declare",
            profile.name
        )));
    }
    let mut changed = Vec::new();
    for (flag, value) in sets {
        let slot = values
            .iter_mut()
            .find(|(name, _)| name == flag)
            .ok_or_else(|| Error::new(format!("--set {flag}: no such flag")))?;
        if slot.1 != *value {
            slot.1.clone_from(value);
            changed.push((flag.clone(), value.clone()));
        }
    }
    let ad_hoc = !changed.is_empty();
    let resolved = Resolved {
        base: profile.name.clone(),
        profile: if ad_hoc {
            "none".to_owned()
        } else {
            profile.name.clone()
        },
        level: if ad_hoc {
            "none".to_owned()
        } else {
            profile.level.clone()
        },
        values,
        changed,
        declared: declared(&file, names, levels),
    };
    consistent(&file, &profile, &resolved)?;
    Ok(resolved)
}

/// Every flag's declared values, with `profile` and `level` first.
fn declared(
    file: &FlagFile,
    profiles: Vec<String>,
    levels: Vec<String>,
) -> Vec<(String, Vec<String>)> {
    let mut declared = vec![
        ("profile".to_owned(), profiles),
        ("level".to_owned(), levels),
    ];
    declared.extend(file.flags.iter().map(|flag| {
        (
            flag.name.clone(),
            flag.values.iter().map(|value| value.name.clone()).collect(),
        )
    }));
    declared
}

/// Every value declared, its requirements met and its conflicts absent, and
/// allowed at the build's level or by the profile's dated exception.
fn consistent(file: &FlagFile, profile: &Profile, resolved: &Resolved) -> Result<()> {
    let has = |(flag, value): &(String, String)| resolved.value(flag) == Some(value.as_str());
    for (flag, value) in &resolved.values {
        let declared = file
            .flag(flag)
            .and_then(|declared| declared.value(value))
            .ok_or_else(|| {
                Error::new(format!(
                    "{flag}={value} is not declared in build-flags.toml"
                ))
            })?;
        if let Some(missing) = declared.requires.iter().find(|pair| !has(pair)) {
            return Err(Error::new(format!(
                "{flag}={value} requires {}={}",
                missing.0, missing.1
            )));
        }
        if let Some(clash) = declared.conflicts.iter().find(|pair| has(pair)) {
            return Err(Error::new(format!(
                "{flag}={value} conflicts with {}={}",
                clash.0, clash.1
            )));
        }
        let excepted = resolved.profile != "none"
            && profile
                .exceptions
                .iter()
                .any(|(name, excepted)| name == flag && excepted == value);
        if !declared.allows.contains(&resolved.level) && !excepted {
            return Err(Error::new(format!(
                "{flag}={value} does not allow level {} (it allows {}): profile {} \
                 is refused (BUILD-FLAGS.md rule 3)",
                resolved.level,
                declared.allows.join(", "),
                resolved.base
            )));
        }
    }
    Ok(())
}

/// Whether every value builds today: built, or a stand-in in a build at
/// level `none`. The refusal names each value's stage (BF8).
fn buildable(resolved: &Resolved) -> Result<()> {
    let file = FlagFile::load()?;
    let mut waiting = Vec::new();
    for (flag, value) in &resolved.values {
        let Some(declared) = file.flag(flag).and_then(|declared| declared.value(value)) else {
            continue;
        };
        match declared.built {
            Built::Yes => {}
            Built::StandIn if resolved.level == "none" => {}
            Built::StandIn => waiting.push(format!(
                "{flag}={value} (a stand-in, which a profile at level {} may not carry; lands at {})",
                resolved.level, declared.lands
            )),
            Built::No => waiting.push(format!("{flag}={value} (lands at {})", declared.lands)),
        }
    }
    if waiting.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "{} is not yet buildable: {}",
        if resolved.profile == "none" {
            format!("an ad-hoc build of profile {}", resolved.base)
        } else {
            format!("profile {}", resolved.profile)
        },
        waiting.join(", ")
    )))
}

/// `--kernel-profile`, if given, and every `--set`.
type Asked = (Option<String>, Vec<(String, String)>);

/// What this run asked for, set once by `main` before anything is built.
static ASKED: OnceLock<Asked> = OnceLock::new();

/// This run's resolution, made the first time a kernel is built.
static RESOLVED: OnceLock<std::result::Result<Resolved, String>> = OnceLock::new();

/// Say what this run asked for. Resolving it waits until a kernel is built,
/// so that a command that builds none never fails on a profile it does not
/// use (`cargo xtask flags --kernel-profile max` shows max, which cannot
/// be built yet).
pub(crate) fn ask(profile: Option<String>, sets: Vec<(String, String)>) {
    let _ = ASKED.set((profile, sets));
}

/// The profile this run named, or the default.
pub(crate) fn asked_profile() -> String {
    ASKED
        .get()
        .and_then(|(profile, _)| profile.clone())
        .unwrap_or_else(|| DEFAULT_PROFILE.to_owned())
}

/// This run's `--set`s.
pub(crate) fn asked_sets() -> Vec<(String, String)> {
    ASKED
        .get()
        .map(|(_, sets)| sets.clone())
        .unwrap_or_default()
}

/// This run's flags, resolved once and checked buildable.
///
/// # Errors
///
/// What [`resolve`] refuses.
pub(crate) fn this_run() -> Result<&'static Resolved> {
    RESOLVED
        .get_or_init(|| {
            resolve(Some(&asked_profile()), &asked_sets()).map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|error| Error::new(error.clone()))
}

/// `cargo xtask flags`: the resolved set for `--kernel-profile` (with any
/// `--set`), what the kernel will be compiled with and the line it will
/// print, and whether it builds today.
pub(crate) fn command() -> Result<()> {
    let resolved = resolve_unbuilt(Some(&asked_profile()), &asked_sets())?;
    let file = FlagFile::load()?;
    println!(
        "profile {} at level {}{}",
        resolved.base,
        resolved.level,
        if resolved.profile == "none" {
            " (changed by --set: an ad-hoc build, no claim)"
        } else {
            ""
        }
    );
    for (flag, value) in &resolved.values {
        let declared = file.flag(flag).and_then(|declared| declared.value(value));
        let note = declared.map_or(String::new(), |declared| {
            let built = match declared.built {
                Built::Yes => String::new(),
                Built::StandIn => format!(
                    ", a stand-in for {} until it lands at {}",
                    declared.stands_in.as_deref().unwrap_or("?"),
                    declared.lands
                ),
                Built::No => format!(", not built until {}", declared.lands),
            };
            format!("allows {}{built}", declared.allows.join(", "))
        });
        let mark = if resolved.changed.iter().any(|(name, _)| name == flag) {
            " (--set)"
        } else {
            ""
        };
        println!("  {flag:<22} {value:<15} {note}{mark}");
    }
    println!("the kernel is compiled with:");
    for word in resolved.cfg_words().split(' ') {
        if let Some((flag, value)) = word.split_once('=') {
            println!("  --cfg ferrix_{flag}=\"{value}\"");
        }
    }
    println!(
        "and prints, computed from those cfgs:\n  {}",
        resolved.boot_line()
    );
    match buildable(&resolved) {
        Ok(()) => println!("buildable today"),
        Err(why) => println!("{why}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Built, FlagFile, resolve, resolve_unbuilt};

    fn set(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(flag, value)| ((*flag).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn present_is_the_default_and_builds_with_its_claim() {
        let resolved = resolve(None, &[]).unwrap();
        assert_eq!(resolved.profile, "present");
        assert_eq!(resolved.level, "present");
        assert_eq!(resolved.value("mitigations"), Some("on"));
        assert!(
            resolved
                .cfg_words()
                .starts_with("profile=present level=present personality=kernel")
        );
    }

    #[test]
    fn a_set_that_changes_a_value_is_ad_hoc_and_one_that_does_not_is_not() {
        let off = resolve(None, &set(&[("mitigations", "off")])).unwrap();
        assert_eq!((off.profile.as_str(), off.level.as_str()), ("none", "none"));
        assert!(off.mitigations_off());
        assert!(off.boot_line().ends_with("; no claim"));
        let same = resolve(None, &set(&[("mitigations", "on")])).unwrap();
        assert_eq!(same.profile, "present");
    }

    #[test]
    fn desktop_builds_and_max_and_fast_are_refused_by_lands() {
        assert_eq!(resolve(Some("desktop"), &[]).unwrap().level, "none");
        for profile in ["max", "fast"] {
            let why = resolve(Some(profile), &[]).unwrap_err().to_string();
            assert!(
                why.contains("not yet buildable") && why.contains("M5"),
                "{why}"
            );
            assert!(resolve_unbuilt(Some(profile), &[]).is_ok());
        }
    }

    #[test]
    fn undeclared_and_unbuilt_values_and_requirements_are_refused() {
        assert!(resolve(None, &set(&[("mitigations", "maybe")])).is_err());
        assert!(resolve(None, &set(&[("no_such_flag", "on")])).is_err());
        let why = resolve(None, &set(&[("fs", "server")]))
            .unwrap_err()
            .to_string();
        assert!(why.contains("not yet buildable"), "{why}");
        let why = resolve(None, &set(&[("personality", "user")]))
            .unwrap_err()
            .to_string();
        assert!(why.contains("requires personality=kernel"), "{why}");
        assert!(resolve(Some("nonesuch"), &[]).is_err());
    }

    #[test]
    fn a_stand_in_builds_only_without_a_claim() {
        let resolved = resolve(None, &set(&[("program_barrier", "session")])).unwrap();
        assert_eq!(resolved.level, "none");
        let file = FlagFile::load().unwrap();
        for flag in &file.flags {
            for value in &flag.values {
                if value.built == Built::StandIn {
                    let instead = value.stands_in.as_deref().and_then(|name| flag.value(name));
                    assert!(
                        instead.is_some_and(|instead| instead.built == Built::Yes),
                        "{}={} stands in for a value that is built",
                        flag.name,
                        value.name
                    );
                }
            }
        }
    }
}
