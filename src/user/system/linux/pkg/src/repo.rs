//! Upgrades from a repository (`docs/UPDATES.md` §2.1): `pkg update`,
//! `pkg newer` and `pkg upgrade`.
//!
//! A repository is a directory with one directory per architecture, each
//! holding `index.toml` and the package files it names. `pkg update` reads
//! this architecture's index, checks it, and keeps it in the root as the
//! accepted index, [`ACCEPTED`]; `newer` and `upgrade` read only the
//! accepted one, so what an upgrade fetches is what `update` checked. An
//! index older than the accepted one is refused: a repository, or whoever
//! stands between it and the machine, cannot take a machine back to an
//! index that lacks a fix it has already seen.
//!
//! Every index and package carries a signature beside it ([`crate::sig`]),
//! verified before anything else is read of it.
//!
//! An upgrade checks every file it fetches against the accepted index --
//! its size, its SHA-256, its signature, and that its record is the package and version
//! the index names, with the dependencies the index resolved -- before
//! [`crate::install_replacing`] makes any of its own checks, and before one
//! file is written.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferrix_pkg::index::{self, Entry, Index};
use ferrix_pkg::record::Record;
use ferrix_sigverify::Anchor;

use crate::{Error, Package, install_replacing, installed, read, sig, this_arch, under};

/// The file naming the repository, from the root: its first line.
pub const REPOSITORY: &str = "etc/pkg/repository";

/// The accepted index, from the root.
pub const ACCEPTED: &str = "var/lib/pkg/index.toml";

/// An index's file name in an architecture's directory.
pub const INDEX: &str = "index.toml";

fn refuse<T>(text: String) -> Result<T, Error> {
    Err(Error(text))
}

/// The repository: `given`, or the first line of the root's
/// [`REPOSITORY`]. A directory, written as a path or a `file://` URL.
///
/// # Errors
///
/// No repository named, or one this `pkg` cannot read from.
pub fn repository(root: &Path, given: Option<&str>) -> Result<PathBuf, Error> {
    let named = match given {
        Some(text) => text.to_owned(),
        None => {
            let file = under(root, REPOSITORY);
            let text = match fs::read_to_string(&file) {
                Ok(text) => text,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return refuse(format!(
                        "no repository: name one with --repo, or in /{REPOSITORY}"
                    ));
                }
                Err(error) => return refuse(format!("{}: {error}", file.display())),
            };
            text.lines().next().unwrap_or_default().trim().to_owned()
        }
    };
    if named.starts_with("http://") || named.starts_with("https://") {
        return refuse(format!(
            "{named}: fetching over HTTP is not built yet (docs/UPDATES.md §4); \
             name a directory"
        ));
    }
    let dir = named.strip_prefix("file://").unwrap_or(&named);
    if dir.is_empty() {
        return refuse(format!("/{REPOSITORY} names no repository"));
    }
    Ok(PathBuf::from(dir))
}

/// The bytes of `file` in `repository`'s directory for this architecture.
fn fetch(repository: &Path, file: &str) -> Result<Vec<u8>, Error> {
    let path = repository.join(this_arch()).join(file);
    fs::read(&path).map_err(|error| Error(format!("{}: {error}", path.display())))
}

/// The accepted index and its bytes, if one has been accepted.
///
/// # Errors
///
/// An accepted index that cannot be read or does not parse.
pub fn accepted(root: &Path) -> Result<Option<(Index, Vec<u8>)>, Error> {
    let path = under(root, ACCEPTED);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return refuse(format!("{}: {error}", path.display())),
    };
    let index = parse(&bytes).map_err(|Error(why)| Error(format!("/{ACCEPTED}: {why}")))?;
    Ok(Some((index, bytes)))
}

/// `bytes` read as an index for this machine.
fn parse(bytes: &[u8]) -> Result<Index, Error> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error("an index is not text".to_owned()))?;
    let index = index::parse(text).map_err(|error| Error(error.to_string()))?;
    if index.arch != this_arch() {
        return refuse(format!(
            "the index is for {}, and this machine is {}",
            index.arch,
            this_arch()
        ));
    }
    Ok(index)
}

/// What `pkg update` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Updated {
    /// The repository's index is the accepted one already.
    Same(Index),
    /// The repository's index was accepted, in the place of the sequence
    /// accepted before, if any.
    Accepted(Index, Option<u64>),
}

/// `file`'s signature in `repository`, or the refusal that says it has
/// none.
fn signature(repository: &Path, file: &str, what: &str) -> Result<Vec<u8>, Error> {
    let name = format!("{file}{}", sig::SUFFIX);
    match fs::read(repository.join(this_arch()).join(&name)) {
        Ok(block) => Ok(block),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            refuse(format!("{what} has no signature, {name}: refused"))
        }
        Err(error) => refuse(format!("{name}: {error}")),
    }
}

/// `pkg update`: read `repository`'s index for this architecture and its
/// signature, check both, and keep the index as the accepted one.
///
/// The signature is verified under `anchors` before the index is read,
/// with the accepted index's sequence as the floor, and the sequence the
/// index says must be the one it signs.
///
/// # Errors
///
/// An index or signature that cannot be read, a signature refused, an index
/// that does not parse, is for another architecture, is older than the
/// accepted one, or has its sequence and other contents; or a file that
/// could not be written.
pub fn update(root: &Path, repository: &Path, anchors: &[Anchor]) -> Result<Updated, Error> {
    let what = "the repository's index";
    let bytes = fetch(repository, INDEX)?;
    let block = signature(repository, INDEX, what)?;
    let before = accepted(root)?;
    let floor = match &before {
        Some((old, _)) => signed_sequence(old.sequence)?,
        None => 0,
    };
    let signed = sig::check(&block, sig::INDEX_SUBJECT, floor, &bytes, anchors)
        .map_err(|Error(why)| Error(format!("{what}: {why}")))?;
    let index = parse(&bytes).map_err(|Error(why)| Error(format!("the repository's {why}")))?;
    if signed_sequence(index.sequence)? != signed {
        return refuse(format!(
            "{what} says it is {}, and its signature names {signed}: refused",
            index.sequence
        ));
    }
    if let Some((old, old_bytes)) = &before {
        if index.sequence < old.sequence {
            return refuse(format!(
                "the repository's index {} is older than the one accepted, {}: refused",
                index.sequence, old.sequence
            ));
        }
        if index.sequence == old.sequence {
            if bytes == *old_bytes {
                return Ok(Updated::Same(index));
            }
            return refuse(format!(
                "the repository's index {} is not the index {} accepted before: refused",
                index.sequence, old.sequence
            ));
        }
    }
    keep(root, ACCEPTED, &bytes)?;
    Ok(Updated::Accepted(
        index,
        before.map(|(old, _)| old.sequence),
    ))
}

/// An index's sequence as the security version its signature carries.
fn signed_sequence(sequence: u64) -> Result<u32, Error> {
    u32::try_from(sequence).map_err(|_| {
        Error(format!(
            "index {sequence} is past what a signature can name"
        ))
    })
}

/// `bytes` at `path` under `root`, written beside it and renamed into place.
fn keep(root: &Path, path: &str, bytes: &[u8]) -> Result<(), Error> {
    let target = under(root, path);
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| Error(format!("{}: {error}", parent.display())))?;
    }
    let partial = target.with_extension("partial");
    fs::write(&partial, bytes).map_err(|error| Error(format!("{}: {error}", partial.display())))?;
    fs::rename(&partial, &target).map_err(|error| Error(format!("{}: {error}", target.display())))
}

/// The accepted index, or the refusal that says to run `pkg update`.
fn accepted_or_refuse(root: &Path) -> Result<Index, Error> {
    match accepted(root)? {
        Some((index, _)) => Ok(index),
        None => refuse("no index accepted yet: run pkg update".to_owned()),
    }
}

/// What `pkg newer` says: a line per installed package the accepted index
/// has a newer version of, one per security fix that version brings, and a
/// last line counting both.
///
/// # Errors
///
/// No accepted index, or one or a record that does not read.
pub fn newer_lines(root: &Path) -> Result<Vec<String>, Error> {
    let index = accepted_or_refuse(root)?;
    let records = installed(root)?;
    let newer = index.newer(&records);
    let mut lines = Vec::new();
    let mut fixed = 0;
    for package in &newer {
        lines.push(format!(
            "{} {} -> {}{}",
            package.name,
            package.installed,
            package.newest.version,
            if package.fixes.is_empty() {
                ""
            } else {
                "  (security)"
            }
        ));
        for fix in &package.fixes {
            lines.push(format!(
                "  security fix in {}: {}",
                fix.version,
                fix.advisory.as_deref().unwrap_or_default()
            ));
        }
        if !package.fixes.is_empty() {
            fixed += 1;
        }
    }
    lines.push(match newer.len() {
        0 => format!(
            "pkg: every package is the newest of index {}",
            index.sequence
        ),
        count => format!(
            "pkg: {count} package(s) have a newer version, {fixed} with a security fix (index {})",
            index.sequence
        ),
    });
    Ok(lines)
}

/// `entry`'s file, fetched from `repository` with its signature and
/// checked against both.
fn fetched(repository: &Path, entry: &Entry, anchors: &[Anchor]) -> Result<Package, Error> {
    let what = format!("{} {}", entry.name, entry.version);
    let bytes = fetch(repository, &entry.file)?;
    let block = signature(repository, &entry.file, &what)?;
    if u64::try_from(bytes.len()).ok() != Some(entry.size) {
        return refuse(format!(
            "{what}: {} is {} bytes, and the index says {}: refused",
            entry.file,
            bytes.len(),
            entry.size
        ));
    }
    if index::sha256(&bytes) != entry.sha256 {
        return refuse(format!(
            "{what}: {} is not the file the index names (its SHA-256 differs): refused",
            entry.file
        ));
    }
    let _ = sig::check(&block, &entry.name, 0, &bytes, anchors)
        .map_err(|Error(why)| Error(format!("{what}: {why}")))?;
    let package = read(&bytes).map_err(|Error(why)| Error(format!("{what}: {why}")))?;
    let found = &package.record.package;
    if found.name != entry.name || found.version != entry.version || found.depends != entry.depends
    {
        return refuse(format!(
            "{what}: {} holds {} {}, which is not the package the index names: refused",
            entry.file, found.name, found.version
        ));
    }
    Ok(package)
}

/// `pkg upgrade`: the newest version of each package `names` names (every
/// installed one when it names none) that the accepted index has, with what
/// it needs, fetched from `repository`, checked, and put in place together.
/// Returns what went in, each with the version it replaced, if any.
///
/// # Errors
///
/// No accepted index, a name not installed or not in it, a dependency
/// neither the root nor the index has, a file not the one the index names
/// or with no signature `anchors` verify for it, or any refusal of
/// [`install_replacing`]'s.
pub fn upgrade(
    root: &Path,
    repository: &Path,
    names: &[String],
    anchors: &[Anchor],
) -> Result<Vec<(Record, Option<Record>)>, Error> {
    let index = accepted_or_refuse(root)?;
    let records = installed(root)?;
    let chosen = index
        .upgrade(&records, names)
        .map_err(|error| Error(error.to_string()))?;
    let packages = chosen
        .into_iter()
        .map(|entry| fetched(repository, entry, anchors))
        .collect::<Result<Vec<_>, _>>()?;
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    install_replacing(root, packages)
}
