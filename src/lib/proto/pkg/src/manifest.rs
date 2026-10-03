//! An app's `app.toml` (`docs/APPS.md` §3): what the package is, and how the
//! tree builds and judges it.
//!
//! Read strictly. A key this module does not know is an error, not ignored,
//! because a misspelt `defualt = true` that is ignored is an app silently
//! left out of every image.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;

use crate::toml::{self, Document, Table, Value};

/// The architectures a package may be built for, by the names xtask uses.
pub const ARCHES: [&str; 3] = ["x86_64", "aarch64", "armv7a"];

/// Why a manifest or a record is refused: a sentence naming the field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<toml::Error> for Error {
    fn from(error: toml::Error) -> Self {
        Self(format!("{error}"))
    }
}

/// A refusal, as an `Err`.
pub(crate) fn refuse<T>(what: String) -> Result<T, Error> {
    Err(Error(what))
}

/// A version: numbers joined by dots, compared number by number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    parts: Vec<u32>,
    text: String,
}

impl Version {
    /// `text`, if it is one to four numbers joined by dots.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Option<Vec<u32>> = text
            .split('.')
            .map(|part| {
                (!part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| part.parse().ok())
                    .flatten()
            })
            .collect();
        let parts = parts.filter(|parts| (1..=4).contains(&parts.len()))?;
        Some(Self {
            parts,
            text: text.to_owned(),
        })
    }

    /// As written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl Ord for Version {
    /// Number by number, a missing number being zero: `1.2` is `1.2.0`.
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.parts.len().max(other.parts.len());
        (0..length)
            .map(|at| {
                let mine = self.parts.get(at).copied().unwrap_or(0);
                let theirs = other.parts.get(at).copied().unwrap_or(0);
                mine.cmp(&theirs)
            })
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// `name`, or `name >= version`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// The package needed.
    pub name: String,
    /// The oldest version that will do, if any will not.
    pub at_least: Option<Version>,
}

impl Dependency {
    /// `text`, as a manifest writes one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let (name, at_least) = match text.split_once(">=") {
            Some((name, version)) => (name.trim(), Some(Version::parse(version.trim())?)),
            None => (text.trim(), None),
        };
        is_name(name).then(|| Self {
            name: name.to_owned(),
            at_least,
        })
    }

    /// Whether `version` of the package will do.
    #[must_use]
    pub fn accepts(&self, version: &Version) -> bool {
        self.at_least.as_ref().is_none_or(|least| version >= least)
    }
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.at_least {
            Some(version) => write!(f, "{} >= {version}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// Which ABI a package's programs speak.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    /// Ferrix's own, through the runtime.
    Native,
    /// Linux's.
    Linux,
}

impl Abi {
    /// As a manifest writes it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Linux => "linux",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "native" => Some(Self::Native),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }
}

/// What the package is: `app.toml`'s `[package]`, which travels into the
/// built package and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Its name, which is its folder's.
    pub name: String,
    /// Its version.
    pub version: Version,
    /// One line.
    pub description: String,
    /// The licence the package's files are under, as an SPDX expression:
    /// `MIT`, `GPL-2.0-only`, `MIT OR Apache-2.0`. For a ported program it
    /// is the program's own, not the recipe's.
    pub license: String,
    /// Its ABI.
    pub abi: Abi,
    /// The architectures it is built for.
    pub arches: Vec<String>,
    /// What must be installed with it.
    pub depends: Vec<Dependency>,
}

/// One of `[[package.files]]`: a file the build makes, or one kept in the
/// app's folder, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    /// A cargo binary's name, or a path in a script's output; with
    /// [`Source::Folder`], a path in the app's folder.
    pub from: String,
    /// Where `from` is.
    pub source: Source,
    /// `tree = true`: `from` is a directory, taken whole -- its files, its
    /// directories and its symbolic links, in name order. A file in it is
    /// 755 when it is a program or a script and 644 otherwise, so a tree
    /// has no `mode`.
    pub tree: bool,
    /// Its path from the root, with no leading `/`.
    pub to: String,
    /// Its permission bits.
    pub mode: u32,
}

/// Where a file of `[[package.files]]` comes from: `source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// What the build made: the default.
    Build,
    /// `source = "folder"`: kept in the app's folder, as written -- a
    /// launcher's `.desktop` file, an icon.
    Folder,
}

/// A tree's directories' permission bits.
pub const TREE_MODE: u32 = 0o755;

/// How the tree builds a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// `cargo build` in the folder.
    Cargo,
    /// `bash build.sh <arch> <out>` in the folder.
    Script,
}

/// A command run in the guest, and the start of a line it must print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Smoke {
    /// The command line.
    pub run: String,
    /// What one line of its output starts with.
    pub expect: String,
}

/// The whole of an `app.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// `[package]`.
    pub package: Package,
    /// `[[package.files]]`.
    pub files: Vec<FileSpec>,
    /// `[build]`'s `kind`.
    pub build: Build,
    /// `[image]`'s `default`: in every image a person runs.
    pub default: bool,
    /// `[check]`'s `host-tests`: the lib target is tested on the host.
    pub host_tests: bool,
    /// `[[smoke]]`.
    pub smoke: Vec<Smoke>,
}

/// Read an `app.toml`.
///
/// # Errors
///
/// Text that is not the TOML subset, a table or key this does not know, a
/// field missing or of the wrong kind, and a value that is not one a
/// package may have: a name, version, architecture, dependency, path or
/// mode.
pub fn recipe(text: &str) -> Result<Recipe, Error> {
    let document = toml::parse(text)?;
    known_tables(&document)?;
    let package = document
        .table("package")
        .ok_or_else(|| Error("there is no [package]".to_owned()))?;
    let package = read_package(package)?;
    let files = document
        .array("package.files")
        .map(read_file)
        .collect::<Result<Vec<_>, _>>()?;
    if files.is_empty() {
        return refuse("[[package.files]]: a package installs at least one file".to_owned());
    }
    no_path_twice(files.iter().map(|file| file.to.as_str()))?;
    let build = match document.table("build") {
        None => Build::Cargo,
        Some(table) => {
            known_keys(table, &["kind"])?;
            match optional_string(table, "kind")?.as_deref() {
                None | Some("cargo") => Build::Cargo,
                Some("script") => Build::Script,
                Some(other) => {
                    return refuse(format!("[build] kind: `{other}` is not cargo or script"));
                }
            }
        }
    };
    let default = flag(document.table("image"), "default", true)?;
    let host_tests = flag(document.table("check"), "host-tests", false)?;
    let smoke = document
        .array("smoke")
        .map(|table| {
            known_keys(table, &["run", "expect"])?;
            Ok(Smoke {
                run: string(table, "run")?,
                expect: string(table, "expect")?,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;
    Ok(Recipe {
        package,
        files,
        build,
        default,
        host_tests,
        smoke,
    })
}

/// Every table is one `app.toml` has.
fn known_tables(document: &Document) -> Result<(), Error> {
    for table in &document.tables {
        let known = match (table.name.as_str(), table.array) {
            ("", false) => table.entries.is_empty(),
            ("package" | "build" | "image" | "check", false)
            | ("package.files" | "smoke", true) => true,
            _ => false,
        };
        if !known {
            return refuse(format!(
                "line {}: `{}` is not a table an app.toml has",
                table.line, table.name
            ));
        }
    }
    Ok(())
}

/// Whether `text` reads as an SPDX licence expression: identifiers made of
/// letters, digits, `.`, `-` and a trailing `+`, joined by `AND`, `OR` and
/// `WITH`, in balanced brackets. The identifiers themselves are not checked
/// against SPDX's list, which moves; the shape is what a reader needs.
fn is_license(text: &str) -> bool {
    let spaced = text.replace('(', " ( ").replace(')', " ) ");
    let mut depth = 0_u32;
    let mut want_term = true;
    for word in spaced.split_whitespace() {
        match word {
            "(" if want_term => depth += 1,
            ")" if !want_term && depth > 0 => depth -= 1,
            "AND" | "OR" | "WITH" if !want_term => want_term = true,
            _ if want_term && is_license_id(word) => want_term = false,
            _ => return false,
        }
    }
    !want_term && depth == 0
}

/// One SPDX identifier: `MIT`, `GPL-2.0-or-later`, `LicenseRef-x`, `GPL-2.0+`.
fn is_license_id(word: &str) -> bool {
    let body = word.strip_suffix('+').unwrap_or(word);
    body.starts_with(|c: char| c.is_ascii_alphabetic())
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// `[package]`.
pub(crate) fn read_package(table: &Table) -> Result<Package, Error> {
    known_keys(
        table,
        &[
            "name",
            "version",
            "description",
            "license",
            "abi",
            "arches",
            "depends",
        ],
    )?;
    let license = string(table, "license")?;
    if !is_license(&license) {
        return refuse(format!(
            "[package] license: `{license}` is not an SPDX expression, such as `MIT` or \
             `MIT OR Apache-2.0`"
        ));
    }
    let name = string(table, "name")?;
    if !is_name(&name) {
        return refuse(format!(
            "[package] name: `{name}` is not lower-case letters, digits and `-`, from a letter"
        ));
    }
    let version = string(table, "version")?;
    let version = Version::parse(&version).ok_or_else(|| {
        Error(format!(
            "[package] version: `{version}` is not numbers and dots"
        ))
    })?;
    let abi = string(table, "abi")?;
    let abi = Abi::parse(&abi)
        .ok_or_else(|| Error(format!("[package] abi: `{abi}` is not native or linux")))?;
    let arches = array(table, "arches")?;
    if arches.is_empty() {
        return refuse("[package] arches: at least one".to_owned());
    }
    if let Some(unknown) = arches.iter().find(|arch| !ARCHES.contains(&arch.as_str())) {
        return refuse(format!(
            "[package] arches: `{unknown}` is not one of {ARCHES:?}"
        ));
    }
    let depends = match table.get("depends") {
        None => Vec::new(),
        Some(_) => array(table, "depends")?
            .iter()
            .map(|text| {
                Dependency::parse(text).ok_or_else(|| {
                    Error(format!(
                        "[package] depends: `{text}` is not `name` or `name >= version`"
                    ))
                })
            })
            .collect::<Result<_, _>>()?,
    };
    Ok(Package {
        name,
        version,
        description: string(table, "description")?,
        license,
        abi,
        arches,
        depends,
    })
}

/// One of `[[package.files]]`.
fn read_file(table: &Table) -> Result<FileSpec, Error> {
    known_keys(table, &["from", "source", "tree", "to", "mode"])?;
    let to = string(table, "to")?;
    if !is_safe_path(&to) {
        return refuse(format!(
            "[[package.files]] to: `{to}` is not a path from the root without `.`, `..` or a leading `/`"
        ));
    }
    let from = string(table, "from")?;
    let source = match optional_string(table, "source")?.as_deref() {
        None | Some("build") => Source::Build,
        Some("folder") => Source::Folder,
        Some(other) => {
            return refuse(format!(
                "[[package.files]] source: `{other}` is not `build` or `folder`"
            ));
        }
    };
    // A file of the folder is the folder's: never one beside it.
    if source == Source::Folder && !is_safe_path(&from) {
        return refuse(format!(
            "[[package.files]] from: `{from}` is not a path in the app's folder without `.`, `..` or a leading `/`"
        ));
    }
    let tree = match table.get("tree") {
        None => false,
        Some(Value::Bool(tree)) => *tree,
        Some(_) => return refuse("[[package.files]] tree: true or false".to_owned()),
    };
    let mode = match (tree, optional_string(table, "mode")?) {
        (false, Some(text)) => mode(&text)?,
        (false, None) => return refuse(format!("[[package.files]] `{to}` has no `mode`")),
        (true, None) => TREE_MODE,
        (true, Some(_)) => {
            return refuse(format!(
                "[[package.files]] `{to}` is a tree, whose files' modes are what they are, so it has no `mode`"
            ));
        }
    };
    Ok(FileSpec {
        from,
        source,
        tree,
        to,
        mode,
    })
}

/// No two of `paths` are the same.
pub(crate) fn no_path_twice<'a>(paths: impl Iterator<Item = &'a str>) -> Result<(), Error> {
    let mut seen: Vec<&str> = Vec::new();
    for path in paths {
        if seen.contains(&path) {
            return refuse(format!("`{path}` is installed twice"));
        }
        seen.push(path);
    }
    Ok(())
}

/// Permission bits written in octal.
pub(crate) fn mode(text: &str) -> Result<u32, Error> {
    u32::from_str_radix(text, 8)
        .ok()
        .filter(|mode| *mode <= 0o7777 && !text.is_empty())
        .ok_or_else(|| {
            Error(format!(
                "mode `{text}` is not permission bits in octal, such as 755"
            ))
        })
}

/// Whether `name` may name a package: lower-case letters, digits and `-`,
/// starting with a letter.
#[must_use]
pub fn is_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Whether `path` names a place under the root and nowhere else: relative,
/// and no component empty, `.` or `..`.
#[must_use]
pub fn is_safe_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

/// Every key of `table` is one of `known`.
pub(crate) fn known_keys(table: &Table, known: &[&str]) -> Result<(), Error> {
    match table
        .entries
        .iter()
        .find(|(key, _)| !known.contains(&key.as_str()))
    {
        Some((key, _)) => refuse(format!(
            "line {}: [{}] has no key `{key}`",
            table.line, table.name
        )),
        None => Ok(()),
    }
}

/// The string `key`, which must be there.
pub(crate) fn string(table: &Table, key: &str) -> Result<String, Error> {
    optional_string(table, key)?.ok_or_else(|| Error(format!("[{}] has no `{key}`", table.name)))
}

/// The string `key`, if it is there.
fn optional_string(table: &Table, key: &str) -> Result<Option<String>, Error> {
    match table.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => refuse(format!("[{}] {key}: a string", table.name)),
    }
}

/// The array of strings `key`, which must be there.
pub(crate) fn array(table: &Table, key: &str) -> Result<Vec<String>, Error> {
    match table.get(key) {
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => refuse(format!("[{}] {key}: an array of strings", table.name)),
        None => refuse(format!("[{}] has no `{key}`", table.name)),
    }
}

/// The flag `key` of a table that may be absent, `absent` when either is.
fn flag(table: Option<&Table>, key: &str, absent: bool) -> Result<bool, Error> {
    let Some(table) = table else {
        return Ok(absent);
    };
    known_keys(table, &[key])?;
    match table.get(key) {
        None => Ok(absent),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => refuse(format!("[{}] {key}: true or false", table.name)),
    }
}
