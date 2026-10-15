//! `check`'s test binaries outside the certified item, each a unit of the
//! step cache (`docs/TEST-TIME.md`, *Cut B5*; the certification
//! consultant's design review, ledger line 1594, P2).
//!
//! A test binary is a file whose bytes are all of its compiled code: an
//! edit that does not reach it leaves it byte for byte the same. With
//! `FERRIX_STEP_CACHE=on`, `check` writes [`FILE`] into the test runs'
//! directory before `cargo test` starts: the step cache's shared prelude
//! (xtask's digest, the host, the origin), and which packages' binaries may
//! be cached and why the others may not. `test-wait` then keys each
//! cacheable binary's run on the binary's SHA-256, its arguments, its
//! directory, the `CARGO_BIN_EXE_*` programs it is given (by their digests),
//! its environment and the host, and, as for every unit, on what `strace`
//! saw it read. A record of a pass on the same key and inputs stands for
//! the run (`cached pass <origin>`); otherwise it runs under `strace` and is
//! recorded when it passes and its trace allows.
//!
//! Never cached (each with its reason in the log, as `ran (never cached:
//! <why>)`): a binary of the item -- a package under a path of the item
//! (`step_cache::item_paths`: the kernel, the loader, every crate
//! `certification-item.json` puts in the item or the core, and its
//! infrastructure allow-list) or one any of them links, through its
//! dependencies or build dependencies on any target, which takes in the
//! libraries the kernel links that the classification does not name yet
//! (finding F-76); a package whose manifest this does not read whole; a
//! package that depends on a crate drawing new inputs each run
//! ([`RANDOMIZED`]) or that seeds its tests from entropy or the clock
//! ([`SEEDED`]); a binary whose package cargo's environment does not name,
//! or names otherwise than the plan does. Release runs and the reference
//! configuration's baseline keep the cache off, as for every unit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::step_cache::{self, Context, Keyed};
use crate::{Error, Result, sha256};

/// The file in the test runs' directory that turns caching on for them.
pub(crate) const FILE: &str = "step-cache";

/// Its first line, with the format's version.
const MAGIC: &str = "ferrix-test-cache 1";

/// Crates whose purpose is to draw new inputs on each run: a package that
/// depends on one, in any kind of dependency, is never cached, since every
/// run of its tests is a new sample (the consultant's P2.3). None is in the
/// workspace today.
pub(crate) const RANDOMIZED: &[&str] = &[
    "arbitrary",
    "bolero",
    "fastrand",
    "getrandom",
    "libfuzzer-sys",
    "oorandom",
    "proptest",
    "quickcheck",
    "rand",
    "rand_chacha",
    "rand_core",
    "rand_xorshift",
];

/// Packages whose tests seed themselves from entropy or the clock without
/// the seed in the key, each with where: never cached (P2.3). Found by
/// reading the sources of every package the item does not link for
/// `RandomState`, `getrandom`, `/dev/urandom`, `SystemTime` and `Instant`
/// on 2026-10-15: none seeds a test from them (ferrix-sync's tests do, and
/// it is the item's).
const SEEDED: &[(&str, &str)] = &[];

/// Packages whose binaries may be cached although an open flake row in
/// `docs/BACKLOG.md` names one of their tests, each with that row, whose
/// flake is a false failure under load (the consultant's ruling, ledger
/// line 1461). The log prints the row beside the cached pass.
pub(crate) const FLAKY_BUT_CACHED: &[(&str, &str)] = &[(
    "xtask",
    "BACKLOG P1 flakes: xtask's gateway::tests::probes_a_shut_window_and_resumes_when_it_opens, 3 us short of its 400 ms under load, a false fail",
)];

/// One workspace member, as its manifest says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    /// Its package name.
    pub(crate) name: String,
    /// Its directory, relative to the root, without a trailing `/`.
    pub(crate) directory: String,
    /// The packages it links: its dependencies and build dependencies,
    /// on every target.
    pub(crate) linked: BTreeSet<String>,
    /// Every package it names, its dev-dependencies too.
    pub(crate) named: BTreeSet<String>,
    /// What of its manifest this could not read, when something.
    pub(crate) unread: Option<String>,
}

/// The workspace's members at `root`.
///
/// # Errors
///
/// A manifest that cannot be read, or a member without a package name.
pub(crate) fn members(root: &Path) -> Result<Vec<Member>> {
    let manifest = read(&root.join("Cargo.toml"))?;
    let mut found = Vec::new();
    for directory in crate::workspace::member_paths(&manifest)? {
        let text = read(&root.join(&directory).join("Cargo.toml"))?;
        let name = crate::workspace::package_name(&text).ok_or_else(|| {
            Error::new(format!(
                "workspace member `{directory}` has no package name"
            ))
        })?;
        let (linked, named, unread) = dependencies(&text);
        found.push(Member {
            name,
            directory: directory.trim_end_matches('/').to_owned(),
            linked,
            named,
            unread,
        });
    }
    Ok(found)
}

/// A file's contents, or an error naming it.
fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))
}

/// The packages a member's manifest links and names, and what of it this
/// could not read: a dependency table's header or entry of a shape it does
/// not know fails closed, so the member is never cached.
pub(crate) fn dependencies(manifest: &str) -> (BTreeSet<String>, BTreeSet<String>, Option<String>) {
    let mut linked = BTreeSet::new();
    let mut named = BTreeSet::new();
    let mut unread = None;
    // The kind of the table the line is in: None outside dependencies.
    let mut table: Option<bool> = None;
    for line in manifest.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_end_matches(']').trim();
            let last = header.rsplit('.').next().unwrap_or_default();
            table = match last {
                "dependencies" | "build-dependencies" => Some(true),
                "dev-dependencies" => Some(false),
                _ if header.contains("dependencies") => {
                    // `[dependencies.<name>]` and the like.
                    let _ = unread.get_or_insert_with(|| format!("the table [{header}]"));
                    None
                }
                _ => None,
            };
            continue;
        }
        let Some(links) = table else {
            continue;
        };
        let Some((key, value)) = line.split_once('=') else {
            let _ = unread.get_or_insert_with(|| format!("the line `{line}`"));
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let name = key
            .split('.')
            .next()
            .unwrap_or_default()
            .trim()
            .trim_matches('"');
        // A table on one line may rename the package.
        let package = if value.starts_with('{') {
            if !value.ends_with('}') {
                let _ = unread.get_or_insert_with(|| format!("the line `{line}`"));
            }
            value
                .trim_matches(|c| c == '{' || c == '}')
                .split(',')
                .find_map(|pair| {
                    let (key, value) = pair.split_once('=')?;
                    (key.trim() == "package").then(|| value.trim().trim_matches('"'))
                })
                .unwrap_or(name)
        } else {
            name
        };
        if package.is_empty() {
            let _ = unread.get_or_insert_with(|| format!("the line `{line}`"));
            continue;
        }
        let _ = named.insert(package.to_owned());
        if links {
            let _ = linked.insert(package.to_owned());
        }
    }
    (linked, named, unread)
}

/// Each member's verdict, by package: `None` when its binaries may be
/// cached, otherwise why not. `item` holds the item's paths as
/// `step_cache::item_paths` gives them, each ending in `/`.
pub(crate) fn verdicts(members: &[Member], item: &[String]) -> BTreeMap<String, Option<String>> {
    let by_name: BTreeMap<&str, &Member> = members
        .iter()
        .map(|member| (member.name.as_str(), member))
        .collect();
    // The item's packages, then everything they link, breadth first, so
    // that each says the shortest way it is the item's.
    let mut why: BTreeMap<String, String> = BTreeMap::new();
    let mut queue: std::collections::VecDeque<(String, String)> = std::collections::VecDeque::new();
    for member in members {
        let directory = format!("{}/", member.directory);
        if let Some(path) = item
            .iter()
            .find(|path| directory.starts_with(path.as_str()))
        {
            queue.push_back((member.name.clone(), format!("the item's ({path})")));
        }
    }
    while let Some((name, reason)) = queue.pop_front() {
        if why.contains_key(&name) {
            continue;
        }
        if let Some(member) = by_name.get(name.as_str()) {
            for linked in &member.linked {
                queue.push_back((linked.clone(), format!("linked by the item's {name}")));
            }
        }
        let _ = why.insert(name, reason);
    }
    members
        .iter()
        .map(|member| {
            let verdict = if let Some(reason) = why.get(&member.name) {
                Some(reason.clone())
            } else if let Some(unread) = &member.unread {
                Some(format!("its manifest was not read whole: {unread}"))
            } else if let Some(crate_name) = member
                .named
                .iter()
                .find(|named| RANDOMIZED.contains(&named.as_str()))
            {
                Some(format!(
                    "it depends on {crate_name}, which draws new inputs each run"
                ))
            } else {
                SEEDED
                    .iter()
                    .find(|(name, _)| *name == member.name)
                    .map(|(_, reason)| format!("its tests seed from {reason}"))
            };
            (member.name.clone(), verdict)
        })
        .collect()
}

/// The text of [`FILE`] for a `check` at `root`, or `None` when the step
/// cache is off or cannot trace here.
///
/// # Errors
///
/// The manifests, the item's classification or this process's context not
/// read.
pub(crate) fn plan(root: &Path) -> Result<Option<String>> {
    let Some(store) = step_cache::store() else {
        return Ok(None);
    };
    if !cfg!(target_os = "linux") || !step_cache::strace_works() {
        return Ok(None);
    }
    let members = members(root)?;
    let item = step_cache::item_paths(root)?;
    let mut text = format!("{MAGIC}\n{}store {}\n", Context::shared()?, store.display());
    for member in &members {
        match verdicts(&members, &item).remove(&member.name).flatten() {
            None => text.push_str(&format!("cache {} {}\n", member.name, member.directory)),
            Some(why) => text.push_str(&format!("never {} {why}\n", member.name)),
        }
    }
    Ok(Some(text))
}

/// What `test-wait` does with one binary.
#[derive(Debug)]
pub(crate) enum Way {
    /// The cache is off: run it as before, and say nothing.
    Off,
    /// Run it, and say why it is never cached.
    Never(String),
    /// Look for a cached pass, and run it traced on a miss.
    Unit(Box<Unit>),
}

/// A test binary as a unit of the step cache.
#[derive(Debug)]
pub(crate) struct Unit {
    /// Its name: `test:<package>:<deps or examples>/<file name less hash>`.
    pub(crate) name: String,
    /// Where its records are.
    pub(crate) directory: PathBuf,
    /// The context it is judged in, keyed on the binary.
    pub(crate) context: Context,
    /// Its package's flake row, when it has one.
    pub(crate) flake: Option<&'static str>,
    /// The binary's SHA-256.
    pub(crate) binary: String,
}

/// The file in a unit's directory saying that its last traced run, which
/// passed, could not be recorded, and why.
const UNRECORDABLE: &str = "unrecordable";

/// How long such a run keeps the unit's later runs untraced, whatever
/// binary they run: xtask's tests read the item, so no run of them is ever
/// recorded, and each edit of xtask makes a new binary whose traced run
/// took 15 to 34 s where an untraced one took 3 to 8 s, on the tests'
/// critical path. After it, the unit is traced again, once.
const UNRECORDABLE_FOR: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl Unit {
    /// Why a traced run of this unit would not be recorded, when one of it
    /// was not within [`UNRECORDABLE_FOR`]: tracing is slow, and a run that
    /// cannot be recorded gains nothing from it. Untraced, it runs as with
    /// the cache off, and nothing is recorded.
    pub(crate) fn unrecordable(&self) -> Option<String> {
        let memo = self.directory.join(UNRECORDABLE);
        let age = std::fs::metadata(&memo)
            .and_then(|metadata| metadata.modified())
            .ok()?
            .elapsed()
            .unwrap_or_default();
        if age > UNRECORDABLE_FOR {
            return None;
        }
        let text = std::fs::read_to_string(memo).ok()?;
        let (_, why) = text.trim().split_once(' ')?;
        Some(why.to_owned())
    }

    /// Remember what a traced run of this unit that passed said: why it
    /// was not recorded (with the binary's digest, for whoever reads the
    /// memo), or nothing when it was.
    pub(crate) fn remember(&self, said: &str) {
        let memo = self.directory.join(UNRECORDABLE);
        match said.strip_prefix("ran, not recorded: ") {
            Some(why) => {
                let _ = std::fs::write(memo, format!("{} {why}\n", self.binary));
            }
            None => {
                let _ = std::fs::remove_file(memo);
            }
        }
    }
}

/// How `test-wait` treats the binary `command` names, run in `cwd` with
/// this process's environment, given the runs' `directory` and the digests
/// it recorded of the binary and its programs (`<digest> <path>`, the binary
/// first).
pub(crate) fn way(directory: &Path, command: &[String], cwd: &Path, digests: &str) -> Way {
    let Ok(text) = std::fs::read_to_string(directory.join(FILE)) else {
        return Way::Off;
    };
    match unit(&text, command, cwd, digests) {
        Ok(way) => way,
        Err(why) => Way::Never(why),
    }
}

/// [`way`] from the plan's `text`: `Err` says why it is not cached.
fn unit(
    text: &str,
    command: &[String],
    cwd: &Path,
    digests: &str,
) -> std::result::Result<Way, String> {
    let mut lines = text.lines();
    if lines.next() != Some(MAGIC) {
        return Err("the cache's plan does not parse".to_owned());
    }
    let shared: String = lines
        .by_ref()
        .take(3)
        .map(|line| format!("{line}\n"))
        .collect();
    let store = lines
        .next()
        .and_then(|line| line.strip_prefix("store "))
        .ok_or("the cache's plan names no store")?;
    let package = std::env::var("CARGO_PKG_NAME")
        .map_err(|_| "cargo's environment names no package".to_owned())?;
    let manifest = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "cargo's environment names no manifest directory".to_owned())?;
    let mut planned = None;
    for line in lines {
        if let Some(rest) = line.strip_prefix("never ")
            && let Some((name, why)) = rest.split_once(' ')
            && name == package
        {
            return Err(why.to_owned());
        }
        if let Some(rest) = line.strip_prefix("cache ")
            && let Some((name, place)) = rest.split_once(' ')
            && name == package
        {
            planned = Some(place.to_owned());
        }
    }
    let place = planned.ok_or_else(|| format!("the plan does not know the package {package}"))?;
    let context = Context::from_shared(&shared).map_err(|error| error.to_string())?;
    let expected = context.root().join(&place);
    if std::fs::canonicalize(&manifest).ok() != std::fs::canonicalize(&expected).ok() {
        return Err(format!(
            "cargo says {package} is at {manifest}, the plan at {place}"
        ));
    }
    let binary = command.first().ok_or("no test binary")?;
    let (lines, files) = key_lines(&context, command, cwd, digests)?;
    let digest = digests
        .split_whitespace()
        .next()
        .ok_or("no digest of the binary")?
        .to_owned();
    let name = unit_name(&package, Path::new(binary));
    let directory = step_cache::unit_directory(Path::new(store), &name);
    let context = context.keyed(Keyed {
        lines,
        files,
        targets: Vec::new(),
        reads_item: false,
    });
    let flake = FLAKY_BUT_CACHED
        .iter()
        .find(|(name, _)| *name == package)
        .map(|(_, row)| *row);
    Ok(Way::Unit(Box::new(Unit {
        name,
        directory,
        context,
        flake,
        binary: digest,
    })))
}

/// The lines a test binary's run is keyed on beside its environment and
/// its trace, and the files they hold the digests of: the binary, each
/// `CARGO_BIN_EXE_*` program (as `test-wait` hashed them before the run),
/// the arguments and the directory.
pub(crate) fn key_lines(
    context: &Context,
    command: &[String],
    cwd: &Path,
    digests: &str,
) -> std::result::Result<(Vec<String>, Vec<PathBuf>), String> {
    let mut lines = Vec::new();
    let mut files = Vec::new();
    for (index, line) in digests.lines().enumerate() {
        let (digest, path) = line
            .split_once(' ')
            .ok_or_else(|| format!("a digest line that does not parse: {line}"))?;
        let what = if index == 0 {
            "test-binary"
        } else {
            "test-program"
        };
        lines.push(format!("{what} {digest} {}", context.placeholders(path)));
        files.push(std::fs::canonicalize(path).map_err(|error| format!("{path}: {error}"))?);
    }
    if lines.is_empty() {
        return Err("no digest of the binary".to_owned());
    }
    let arguments: Vec<String> = command
        .iter()
        .skip(1)
        .map(|argument| context.placeholders(argument))
        .collect();
    lines.push(format!(
        "test-arguments {}",
        sha256::hex(arguments.join("\0").as_bytes())
    ));
    lines.push(format!("test-directory {}", context.written(cwd)));
    Ok((lines, files))
}

/// A unit's name for the test binary at `binary` of `package`: the
/// directory it is in (`deps`, `examples`) and its file name less cargo's
/// hash.
pub(crate) fn unit_name(package: &str, binary: &Path) -> String {
    let file = binary
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let stem = match file.rsplit_once('-') {
        Some((stem, hash))
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) =>
        {
            stem.to_owned()
        }
        _ => file.clone(),
    };
    let kind = binary
        .parent()
        .and_then(Path::file_name)
        .unwrap_or_default()
        .to_string_lossy();
    format!("test:{package}:{kind}/{stem}")
}

#[cfg(test)]
mod tests;
