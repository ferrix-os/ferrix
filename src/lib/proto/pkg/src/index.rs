//! A repository's index for one architecture (`docs/UPDATES.md` §2.1):
//! every version of every package the repository has, with its file, size,
//! SHA-256 and dependencies, and a security mark and advisory per version.
//!
//! The package manager keeps the index it last accepted, compares the
//! installed records with it ([`Index::newer`]), and chooses what an upgrade
//! fetches ([`Index::upgrade`]): the newest version of each package asked
//! for, and the newest version of each dependency that the root does not
//! already hold in a version that will do -- minimum versions and no solver
//! (`docs/APPS.md` §7). xtask writes an index with [`render`], and the
//! package manager reads it with [`parse`], so the two cannot disagree.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::manifest::{self, ARCHES, Dependency, Error, Version, refuse};
use crate::record::Record;
use crate::toml::{self, Table, Value};

/// What a package file's name ends in.
pub const PACKAGE_SUFFIX: &str = ".fxpkg";

/// The longest advisory, in bytes: one line a terminal shows whole.
pub const ADVISORY_MAX: usize = 200;

/// A file's SHA-256, as an index names it.
#[must_use]
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    ferrix_sha256::digest(bytes)
}

/// One version of one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The package's name.
    pub name: String,
    /// This version.
    pub version: Version,
    /// Its file's name in the repository's directory for the architecture.
    pub file: String,
    /// The file's length in bytes.
    pub size: u64,
    /// The file's SHA-256.
    pub sha256: [u8; 32],
    /// What it depends on, as its record says.
    pub depends: Vec<Dependency>,
    /// Whether this version fixes a security fault.
    pub security: bool,
    /// The fault it fixes, one line; present exactly when `security` is.
    pub advisory: Option<String>,
}

/// An index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index {
    /// The architecture every package in it is built for.
    pub arch: String,
    /// Its place in the repository's publications: a later index has a
    /// higher one.
    pub sequence: u64,
    /// Every package version, in the order written.
    pub entries: Vec<Entry>,
}

/// An installed package that the index has a newer version of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newer<'a> {
    /// The installed version's name.
    pub name: &'a str,
    /// The installed version.
    pub installed: &'a Version,
    /// The newest version the index has.
    pub newest: &'a Entry,
    /// Every version newer than the installed one that is marked a security
    /// fix, oldest first: the fixes an upgrade to `newest` brings.
    pub fixes: Vec<&'a Entry>,
}

impl Index {
    /// The newest version of `name`, if the index has it.
    #[must_use]
    pub fn newest(&self, name: &str) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.name == name)
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// The newest version of `dependency`'s package that will do for it.
    #[must_use]
    pub fn newest_for(&self, dependency: &Dependency) -> Option<&Entry> {
        self.entries
            .iter()
            .filter(|entry| entry.name == dependency.name && dependency.accepts(&entry.version))
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// Each package of `installed` that the index has a newer version of,
    /// in `installed`'s order.
    #[must_use]
    pub fn newer<'a>(&'a self, installed: &'a [Record]) -> Vec<Newer<'a>> {
        installed
            .iter()
            .filter_map(|record| {
                let package = &record.package;
                let newest = self.newest(&package.name)?;
                if newest.version <= package.version {
                    return None;
                }
                let mut fixes: Vec<&Entry> = self
                    .entries
                    .iter()
                    .filter(|entry| {
                        entry.name == package.name
                            && entry.security
                            && entry.version > package.version
                    })
                    .collect();
                fixes.sort_by(|a, b| a.version.cmp(&b.version));
                Some(Newer {
                    name: &package.name,
                    installed: &package.version,
                    newest,
                    fixes,
                })
            })
            .collect()
    }

    /// What an upgrade of `names` fetches, given the `installed` packages:
    /// the newest version of each name (every installed package when
    /// `names` is empty) that is newer than the installed one, and then,
    /// for each dependency of what is chosen that neither the chosen
    /// versions nor the installed ones meet, the newest version that does.
    /// In the order chosen; empty when everything is up to date.
    ///
    /// # Errors
    ///
    /// A name that is not installed or not in the index, or a dependency
    /// that neither the root nor the index has in a version that will do.
    pub fn upgrade<'a>(
        &'a self,
        installed: &[Record],
        names: &[String],
    ) -> Result<Vec<&'a Entry>, Error> {
        let version_of = |name: &str| {
            installed
                .iter()
                .find(|record| record.package.name == name)
                .map(|record| &record.package.version)
        };
        let mut chosen: Vec<&Entry> = Vec::new();
        let asked: Vec<&str> = if names.is_empty() {
            installed
                .iter()
                .map(|record| record.package.name.as_str())
                .collect()
        } else {
            names.iter().map(String::as_str).collect()
        };
        for name in asked {
            let Some(have) = version_of(name) else {
                return refuse(format!("{name} is not installed"));
            };
            let Some(newest) = self.newest(name) else {
                if names.is_empty() {
                    continue;
                }
                return refuse(format!("the repository has no {name}"));
            };
            if newest.version > *have {
                chosen.push(newest);
            }
        }
        // Each chosen version's dependencies, breadth first; every entry is
        // chosen at most once, so this ends within the index's length.
        let mut next = 0;
        while let Some(entry) = chosen.get(next).copied() {
            next += 1;
            for needed in &entry.depends {
                let met_by_chosen = chosen
                    .iter()
                    .any(|c| c.name == needed.name && needed.accepts(&c.version));
                let replaced = chosen.iter().any(|c| c.name == needed.name);
                let met_installed =
                    !replaced && version_of(&needed.name).is_some_and(|v| needed.accepts(v));
                if met_by_chosen || met_installed {
                    continue;
                }
                // The newest version meets a minimum if any does, so a
                // package the upgrade already chose cannot be met by
                // another of its versions.
                match self.newest_for(needed) {
                    Some(found) if !replaced => chosen.push(found),
                    _ => {
                        return refuse(format!(
                            "{} {} needs {needed}, which neither this root nor the repository has",
                            entry.name, entry.version
                        ));
                    }
                }
            }
        }
        Ok(chosen)
    }
}

/// Read an index.
///
/// # Errors
///
/// Text that is not the subset, a table or key an index does not have, a
/// field missing or not what an index writes, and one version of a package
/// listed twice.
pub fn parse(text: &str) -> Result<Index, Error> {
    let document = toml::parse(text)?;
    for table in &document.tables {
        let known = match (table.name.as_str(), table.array) {
            ("", false) => table.entries.is_empty(),
            ("index", false) | ("packages", true) => true,
            _ => false,
        };
        if !known {
            return refuse(format!(
                "line {}: `{}` is not in an index",
                table.line, table.name
            ));
        }
    }
    let head = document
        .table("index")
        .ok_or_else(|| Error("an index has no [index]".to_owned()))?;
    manifest::known_keys(head, &["arch", "sequence"])?;
    let arch = manifest::string(head, "arch")?;
    if !ARCHES.contains(&arch.as_str()) {
        return refuse(format!("[index] arch: `{arch}` is not one of {ARCHES:?}"));
    }
    let sequence = number(head, "sequence")?;
    let entries = document
        .array("packages")
        .map(|table| read_entry(table, &arch))
        .collect::<Result<Vec<_>, _>>()?;
    for (at, entry) in entries.iter().enumerate() {
        if entries
            .iter()
            .skip(at + 1)
            .any(|other| other.name == entry.name && other.version == entry.version)
        {
            return refuse(format!(
                "{} {} is in the index twice",
                entry.name, entry.version
            ));
        }
    }
    Ok(Index {
        arch,
        sequence,
        entries,
    })
}

/// A number written as a string, as records write sizes.
fn number(table: &Table, key: &str) -> Result<u64, Error> {
    let text = manifest::string(table, key)?;
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return refuse(format!("[{}] {key}: `{text}` is not a number", table.name));
    }
    text.parse()
        .map_err(|_| Error(format!("[{}] {key}: `{text}` is not a number", table.name)))
}

fn read_entry(table: &Table, arch: &str) -> Result<Entry, Error> {
    manifest::known_keys(
        table,
        &[
            "name", "version", "file", "size", "sha256", "depends", "security", "advisory",
        ],
    )?;
    let name = manifest::string(table, "name")?;
    if !manifest::is_name(&name) {
        return refuse(format!("[[packages]] name: `{name}` is not a package name"));
    }
    let text = manifest::string(table, "version")?;
    let version = Version::parse(&text)
        .ok_or_else(|| Error(format!("[[packages]] {name}: `{text}` is not a version")))?;
    let file = manifest::string(table, "file")?;
    if !is_file_name(&file) {
        return refuse(format!(
            "[[packages]] {name}: `{file}` is not a package file's name in the {arch} directory"
        ));
    }
    let size = number(table, "size")?;
    let hex = manifest::string(table, "sha256")?;
    let sha256 = digest(&hex).ok_or_else(|| {
        Error(format!(
            "[[packages]] {name}: sha256 `{hex}` is not 32 bytes in lower-case hex"
        ))
    })?;
    let depends = manifest::array(table, "depends")?
        .iter()
        .map(|text| {
            Dependency::parse(text)
                .ok_or_else(|| Error(format!("[[packages]] {name}: `{text}` is not a dependency")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let security = match table.get("security") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return refuse(format!("[[packages]] {name}: security is true or false")),
    };
    let advisory = match table.get("advisory") {
        None => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => return refuse(format!("[[packages]] {name}: advisory is a string")),
    };
    match (&advisory, security) {
        (None, true) => {
            return refuse(format!(
                "[[packages]] {name} {version}: a security fix without an advisory"
            ));
        }
        (Some(_), false) => {
            return refuse(format!(
                "[[packages]] {name} {version}: an advisory on a version that is not a security fix"
            ));
        }
        (Some(text), true)
            if text.is_empty()
                || text.len() > ADVISORY_MAX
                || text.chars().any(char::is_control) =>
        {
            return refuse(format!(
                "[[packages]] {name} {version}: an advisory is one line of 1 to {ADVISORY_MAX} bytes"
            ));
        }
        _ => {}
    }
    Ok(Entry {
        name,
        version,
        file,
        size,
        sha256,
        depends,
        security,
        advisory,
    })
}

/// Whether `file` is a package file's name: one path component, ending in
/// [`PACKAGE_SUFFIX`], of the characters a package's name and version are
/// written in.
#[must_use]
pub fn is_file_name(file: &str) -> bool {
    file.len() > PACKAGE_SUFFIX.len()
        && file.ends_with(PACKAGE_SUFFIX)
        && !file.starts_with('.')
        && file
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// A SHA-256 from lower-case hex.
fn digest(hex: &str) -> Option<[u8; 32]> {
    let mut out = [0; 32];
    if hex.len() != 64 {
        return None;
    }
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let pair = core::str::from_utf8(pair).ok()?;
        if pair.bytes().any(|digit| digit.is_ascii_uppercase()) {
            return None;
        }
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// `bytes` as lower-case hex.
#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing to a `String` does not fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// `index` as its file.
#[must_use]
pub fn render(index: &Index) -> String {
    let mut out = String::new();
    // Writing to a `String` does not fail.
    let _ = write_index(&mut out, index);
    out
}

fn write_index(out: &mut String, index: &Index) -> fmt::Result {
    out.write_str("[index]\narch = ")?;
    toml::write_string(out, &index.arch)?;
    write!(out, "\nsequence = \"{}\"\n", index.sequence)?;
    for entry in &index.entries {
        out.write_str("\n[[packages]]\nname = ")?;
        toml::write_string(out, &entry.name)?;
        out.write_str("\nversion = ")?;
        toml::write_string(out, entry.version.as_str())?;
        out.write_str("\nfile = ")?;
        toml::write_string(out, &entry.file)?;
        write!(
            out,
            "\nsize = \"{}\"\nsha256 = \"{}\"\ndepends = ",
            entry.size,
            hex(&entry.sha256)
        )?;
        let depends: Vec<String> = entry.depends.iter().map(|d| format!("{d}")).collect();
        toml::write_array(out, &depends)?;
        if entry.security {
            out.write_str("\nsecurity = true")?;
        }
        if let Some(advisory) = &entry.advisory {
            out.write_str("\nadvisory = ")?;
            toml::write_string(out, advisory)?;
        }
        out.write_char('\n')?;
    }
    Ok(())
}

/// The entry for `record`'s package as built into `bytes`, its file named
/// as xtask names a package: `<name>-<version>-<arch>.fxpkg`.
#[must_use]
pub fn entry_of(record: &Record, bytes: &[u8]) -> Entry {
    let package = &record.package;
    let arch = package.arches.first().map_or("", String::as_str);
    Entry {
        name: package.name.clone(),
        version: package.version.clone(),
        file: format!(
            "{}-{}-{arch}{PACKAGE_SUFFIX}",
            package.name, package.version
        ),
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        sha256: sha256(bytes),
        depends: package.depends.clone(),
        security: false,
        advisory: None,
    }
}
