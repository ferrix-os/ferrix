//! Ferrix's package manager: what `/bin/pkg` does, on any root.
//!
//! A package is a newc archive of its files and its record,
//! `lib/ferrix/packages/<name>.toml` (`docs/APPS.md` §6); an installed
//! package is its record in the root, beside the files it lists. So this
//! keeps no database: [`installed`] reads the records, [`install`] unpacks
//! a package whose every file its record vouches for, and [`remove`]
//! deletes what a record lists.
//!
//! Every check is made before the first file is written. An install is
//! refused when a package is not for this machine's architecture, when an
//! entry is not what its record says or a file its record lists is missing,
//! when it is installed already (unless it is to replace it,
//! [`install_replacing`]), when what it depends on is not installed or
//! coming with it (`ferrix_pkg::plan`), and when it would put down a path
//! that is there already -- another package's, or the system's. A removal is
//! refused while an installed package depends on it. The record is written
//! last and deleted first, so a package whose files are half there has no
//! record, and is not installed.
//!
//! Every function takes the root, so the host tests run it in a directory.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use ferrix_cpio::{Archive, FileType};
use ferrix_pkg::plan::plan;
use ferrix_pkg::record::{self, Installed, Record};

/// Why something was refused, or failed: the line `pkg` prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// `Err` of `text`.
fn refuse<T>(text: String) -> Result<T, Error> {
    Err(Error(text))
}

/// The architecture this program was built for, as a record names it.
#[must_use]
pub const fn this_arch() -> &'static str {
    if cfg!(target_arch = "x86_64") {
        "x86_64"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "armv7a"
    }
}

/// `path` beneath `root`, a path from the root with no `.` or `..`.
fn under(root: &Path, path: &str) -> PathBuf {
    path.split('/')
        .fold(root.to_path_buf(), |at, part| at.join(part))
}

fn io_error(path: &Path) -> impl Fn(io::Error) -> Error + '_ {
    move |error| Error(format!("{}: {error}", path.display()))
}

/// The packages installed in `root`, by name.
///
/// # Errors
///
/// A record that cannot be read, or does not parse.
pub fn installed(root: &Path) -> Result<Vec<Record>, Error> {
    let dir = under(root, record::RECORDS);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return refuse(format!("{}: {error}", dir.display())),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry.map_err(io_error(&dir))?.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(io_error(&path))?;
        let record =
            record::parse(&text).map_err(|error| Error(format!("{}: {error}", path.display())))?;
        records.push(record);
    }
    records.sort_by(|a, b| a.package.name.cmp(&b.package.name));
    Ok(records)
}

/// What `pkg info` says of `record`: the package, then each file.
#[must_use]
pub fn info_lines(record: &Record) -> Vec<String> {
    let package = &record.package;
    let depends: Vec<String> = package.depends.iter().map(ToString::to_string).collect();
    let mut lines = vec![
        format!("name: {}", package.name),
        format!("version: {}", package.version.as_str()),
        format!("description: {}", package.description),
        format!("abi: {}", package.abi.as_str()),
        format!("arch: {}", package.arches.join(", ")),
        format!("depends: {}", depends.join(", ")),
    ];
    for file in &record.files {
        lines.push(match &file.link {
            Some(target) => format!("  /{} -> {target}", file.path),
            None => format!("  /{} {:o} {}", file.path, file.mode, file.size),
        });
    }
    lines
}

/// One entry of a package: a file's bytes or a link's target.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    /// A regular file.
    Bytes(Vec<u8>),
    /// A symbolic link.
    Link(String),
}

/// A package read and checked: its record and what it puts down.
#[derive(Debug, Clone)]
pub struct Package {
    /// Its record.
    pub record: Record,
    /// Each path it installs, its mode and what is there, in the archive's
    /// order.
    entries: Vec<(String, u32, Content)>,
}

/// Read `archive`, a package, and check that every entry is its record's,
/// with the size and digest the record says, and that every file the record
/// lists is there.
///
/// # Errors
///
/// An archive that does not read, a package without a record or with an
/// entry unlike it, or one for another architecture.
pub fn read(archive: &[u8]) -> Result<Package, Error> {
    let prefix = format!("{}/", record::RECORDS);
    let mut record = None;
    let mut entries = Vec::new();
    for entry in Archive::new(archive).entries() {
        let entry = entry.map_err(|error| Error(format!("not a package: {error:?}")))?;
        let mode = entry.mode & 0o7777;
        match entry.file_type() {
            FileType::Regular if entry.name.starts_with(&prefix) => {
                let text = std::str::from_utf8(entry.data)
                    .map_err(|_| Error(format!("{} is not text", entry.name)))?;
                let parsed = record::parse(text)
                    .map_err(|error| Error(format!("{}: {error}", entry.name)))?;
                if record.replace((entry.name.to_owned(), parsed)).is_some() {
                    return refuse("a package with two records".to_owned());
                }
            }
            FileType::Regular => {
                entries.push((
                    entry.name.to_owned(),
                    mode,
                    Content::Bytes(entry.data.to_vec()),
                ));
            }
            FileType::Symlink => {
                let target = entry
                    .symlink_target()
                    .ok_or_else(|| Error(format!("{}: a link that is not text", entry.name)))?;
                entries.push((
                    entry.name.to_owned(),
                    mode,
                    Content::Link(target.to_owned()),
                ));
            }
            FileType::Directory => {}
            _ => return refuse(format!("{}: not a file, a link or a directory", entry.name)),
        }
    }
    let (at, record) = record.ok_or_else(|| Error("a package without a record".to_owned()))?;
    let name = &record.package.name;
    if record::path(name) != at {
        return refuse(format!("{name}'s record is not at {}", record::path(name)));
    }
    let arch = record.package.arches.first().map_or("", String::as_str);
    if arch != this_arch() {
        return refuse(format!(
            "{name} is built for {arch}, and this machine is {}",
            this_arch()
        ));
    }
    for (path, mode, content) in &entries {
        if !ferrix_cpio::is_safe_path(path) {
            return refuse(format!("{name}'s package holds {path}, outside the root"));
        }
        let found = match content {
            Content::Bytes(bytes) => Installed::of(path, *mode, bytes),
            Content::Link(target) => Installed::link(path, target),
        };
        let listed = record
            .files
            .iter()
            .find(|file| file.path == *path)
            .ok_or_else(|| {
                Error(format!(
                    "{name}'s package holds {path}, which its record does not list"
                ))
            })?;
        if *listed != found {
            return refuse(format!("{name}'s {path} is not what its record says"));
        }
    }
    if let Some(missing) = record
        .files
        .iter()
        .find(|file| !entries.iter().any(|(path, _, _)| *path == file.path))
    {
        return refuse(format!(
            "{name}'s record lists {}, which its package does not hold",
            missing.path
        ));
    }
    Ok(Package { record, entries })
}

/// Install `packages` into `root`, together: each after what it depends
/// on. Returns their records, in the order they went in.
///
/// # Errors
///
/// A package installed already, a dependency neither installed nor among
/// `packages`, a path two packages would own or that is in the root already,
/// or a file that could not be written.
pub fn install(root: &Path, packages: Vec<Package>) -> Result<Vec<Record>, Error> {
    install_with(root, packages, false)
}

/// [`install`], where a package of a name that is installed already takes
/// the installed one's place rather than being refused: `pkg install
/// --replace`. Returns the records that went in, in their order, each with
/// the version it replaced, if any.
///
/// Every check [`install`] makes is made first, against the set as it will
/// be: the installed packages without the replaced ones, and the new ones.
/// A path that is in the root already may be put down only where the
/// package being replaced owns it.
///
/// Then one transaction per package, in an order that leaves something a
/// second `pkg install --replace` of the same file repairs, whenever it
/// stops: every new file is written beside its place first (nothing in use
/// has changed yet, and a failure removes them again), then each is renamed
/// over its place, then each file only the old version had is deleted, and
/// the new record is renamed over the old one last. Until that last rename
/// the old record is the package's, so a replacement that stopped part way
/// is still the old package, installed, and owning every path the new one
/// puts down. A program that is running keeps the file it was started from,
/// which the rename unlinks rather than writes over.
///
/// # Errors
///
/// As [`install`], less "installed already".
pub fn install_replacing(
    root: &Path,
    packages: Vec<Package>,
) -> Result<Vec<(Record, Option<Record>)>, Error> {
    let present = installed(root)?;
    let replaced: Vec<Option<Record>> = packages
        .iter()
        .map(|package| {
            present
                .iter()
                .find(|record| record.package.name == package.record.package.name)
                .cloned()
        })
        .collect();
    let done = install_with(root, packages, true)?;
    Ok(done
        .into_iter()
        .map(|record| {
            let old = replaced
                .iter()
                .flatten()
                .find(|old| old.package.name == record.package.name)
                .cloned();
            (record, old)
        })
        .collect())
}

/// [`install`], and [`install_replacing`] when `replace`.
fn install_with(root: &Path, packages: Vec<Package>, replace: bool) -> Result<Vec<Record>, Error> {
    let present = installed(root)?;
    let replacing = |name: &str| replace && packages.iter().any(|p| p.record.package.name == name);
    if !replace {
        for package in &packages {
            let name = &package.record.package.name;
            if let Some(there) = present.iter().find(|record| record.package.name == *name) {
                return refuse(format!(
                    "{name} {} is installed already; remove it first, or install it with --replace",
                    there.package.version
                ));
            }
        }
    }
    let kept: Vec<Record> = present
        .iter()
        .filter(|record| !replacing(&record.package.name))
        .cloned()
        .collect();
    let old = |name: &str| -> Option<&Record> {
        present
            .iter()
            .find(|record| replacing(name) && record.package.name == name)
    };
    let mut all: Vec<Record> = kept.clone();
    all.extend(packages.iter().map(|package| package.record.clone()));
    let order = plan(&all).map_err(|error| Error(error.to_string()))?;
    for package in &packages {
        let name = &package.record.package.name;
        let owned = old(name);
        for (path, _, _) in &package.entries {
            let theirs = owned.is_some_and(|record| record.files.iter().any(|f| f.path == *path));
            if !theirs && fs::symlink_metadata(under(root, path)).is_ok() {
                return refuse(format!(
                    "{name} would put down /{path}, which is there already"
                ));
            }
        }
    }
    let mut done = Vec::new();
    for at in order {
        let Some(new) = at.checked_sub(kept.len()) else {
            continue;
        };
        let Some(package) = packages.get(new) else {
            continue;
        };
        match old(&package.record.package.name) {
            Some(record) => replace_with(root, record, package)?,
            None => put_down(root, package)?,
        }
        done.push(package.record.clone());
    }
    Ok(done)
}

/// Where a file of a replacement waits beside its place: the same name with
/// [`STAGED`] added, so that no two files of one directory share it.
fn staged(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(STAGED);
    path.with_file_name(name)
}

/// What [`staged`] adds to a name.
const STAGED: &str = ".pkg-new";

/// Put `package` down in `old`'s place, as [`install_replacing`] says.
fn replace_with(root: &Path, old: &Record, package: &Package) -> Result<(), Error> {
    let name = &package.record.package.name;
    let record_at = under(root, &record::path(name));
    let mut waiting: Vec<PathBuf> = Vec::new();
    let unstage = |waiting: &[PathBuf]| {
        for path in waiting {
            let _ = fs::remove_file(staged(path));
        }
    };
    // 1. Every new file beside its place; nothing in use has changed.
    for (path, mode, content) in &package.entries {
        let target = under(root, path);
        if let Some(parent) = target.parent()
            && let Err(error) = fs::create_dir_all(parent)
        {
            unstage(&waiting);
            return refuse(format!("{}: {error}", parent.display()));
        }
        let beside = staged(&target);
        let _ = fs::remove_file(&beside);
        let written = match content {
            Content::Bytes(bytes) => write_plain(&beside, *mode, bytes),
            Content::Link(to) => link(to, &beside),
        };
        if let Err(error) = written {
            unstage(&waiting);
            return Err(error);
        }
        waiting.push(target);
    }
    let rendered = record::render(&package.record);
    if let Err(error) = write_plain(&staged(&record_at), 0o644, rendered.as_bytes()) {
        unstage(&waiting);
        return Err(error);
    }
    // 2. Each over its place. From here a stop leaves the old record, which
    // owns every path, so the same command again finishes the work.
    for target in &waiting {
        fs::rename(staged(target), target).map_err(io_error(target))?;
    }
    // 3. What only the old version had.
    for file in &old.files {
        if package
            .entries
            .iter()
            .any(|(path, _, _)| *path == file.path)
        {
            continue;
        }
        let path = under(root, &file.path);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return refuse(format!("{}: {error}", path.display())),
        }
        let mut at = path;
        while at.pop() && at != root && fs::remove_dir(&at).is_ok() {}
    }
    // 4. The record last: now it is the new version.
    fs::rename(staged(&record_at), &record_at).map_err(io_error(&record_at))
}

/// `bytes` at `path` with `mode`, written in place.
fn write_plain(path: &Path, mode: u32, bytes: &[u8]) -> Result<(), Error> {
    fs::write(path, bytes).map_err(io_error(path))?;
    set_mode(path, mode)
}

/// Write `package`'s files, then its record.
fn put_down(root: &Path, package: &Package) -> Result<(), Error> {
    for (path, mode, content) in &package.entries {
        let target = under(root, path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(io_error(parent))?;
        }
        match content {
            Content::Bytes(bytes) => write_file(&target, *mode, bytes)?,
            Content::Link(to) => link(to, &target)?,
        }
    }
    let name = &package.record.package.name;
    let target = under(root, &record::path(name));
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    write_file(&target, 0o644, record::render(&package.record).as_bytes())
}

/// `bytes` at `path` with `mode`, written beside it and renamed into place.
fn write_file(path: &Path, mode: u32, bytes: &[u8]) -> Result<(), Error> {
    let partial = path.with_extension("pkg-partial");
    fs::write(&partial, bytes).map_err(io_error(&partial))?;
    set_mode(&partial, mode)?;
    fs::rename(&partial, path).map_err(io_error(path))
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(io_error(path))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), Error> {
    Ok(())
}

#[cfg(unix)]
fn link(target: &str, path: &Path) -> Result<(), Error> {
    std::os::unix::fs::symlink(target, path).map_err(io_error(path))
}

#[cfg(not(unix))]
fn link(_target: &str, path: &Path) -> Result<(), Error> {
    refuse(format!(
        "{}: no symbolic links on this host",
        path.display()
    ))
}

/// Remove the package `name` from `root`: its record first, then every file
/// it lists, then each directory that held one and is empty now. Returns
/// its record and the files that were already gone.
///
/// # Errors
///
/// No such package, one an installed package depends on, or a file that
/// could not be deleted.
pub fn remove(root: &Path, name: &str) -> Result<(Record, Vec<String>), Error> {
    let present = installed(root)?;
    let record = present
        .iter()
        .find(|record| record.package.name == name)
        .cloned()
        .ok_or_else(|| Error(format!("{name} is not installed")))?;
    if let Some(user) = present.iter().find(|other| {
        other
            .package
            .depends
            .iter()
            .any(|dependency| dependency.name == name)
    }) {
        return refuse(format!(
            "{} depends on {name}; remove it first",
            user.package.name
        ));
    }
    let record_path = under(root, &record::path(name));
    fs::remove_file(&record_path).map_err(io_error(&record_path))?;
    let mut gone = Vec::new();
    for file in &record.files {
        let path = under(root, &file.path);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => gone.push(file.path.clone()),
            Err(error) => return refuse(format!("{}: {error}", path.display())),
        }
    }
    for file in &record.files {
        let mut at = under(root, &file.path);
        while at.pop() && at != root && fs::remove_dir(&at).is_ok() {}
    }
    Ok((record, gone))
}

#[cfg(test)]
mod tests;
