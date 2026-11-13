//! `pkg`: Ferrix's package manager (`docs/APPS.md` §7).
//!
//! ```text
//! pkg [--root DIR] list               the packages installed
//! pkg [--root DIR] info NAME          one package: what it is and its files
//! pkg [--root DIR] install FILE...    install packages (.fxpkg), together
//! pkg [--root DIR] install --replace FILE...
//!                                     the same, each in the place of the
//!                                     version installed, if one is
//! pkg [--root DIR] remove NAME        remove a package
//! pkg [--root DIR] update [--repo DIR]
//!                                     accept the repository's index
//! pkg [--root DIR] newer              what the accepted index has newer,
//!                                     and the security fixes waiting
//! pkg [--root DIR] upgrade [--repo DIR] [NAME...]
//!                                     fetch, check and put in place the
//!                                     newest of each (every package when
//!                                     none is named), with what it needs
//! ```
//!
//! The repository is `--repo`, or the first line of `/etc/pkg/repository`
//! (`docs/UPDATES.md`).
//!
//! The root is `/` unless `--root` names another. A refusal is one line,
//! `pkg: ` and why, on standard error, and the exit status 1; a command line
//! it does not take is the usage and 2.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;

use pkg::repo::{self, Updated};
use pkg::{Error, info_lines, install, install_replacing, installed, read, remove};

const USAGE: &str = "usage: pkg [--root DIR] list | info NAME | install [--replace] FILE... | \
                     remove NAME | update [--repo DIR] | newer | upgrade [--repo DIR] [NAME...]";

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = PathBuf::from("/");
    if args.first().map(String::as_str) == Some("--root") {
        let Some(dir) = args.get(1).cloned() else {
            return usage();
        };
        root = PathBuf::from(dir);
        let _ = args.drain(..2);
    }
    let Some((command, rest)) = args.split_first() else {
        return usage();
    };
    let done = match (command.as_str(), rest) {
        ("list", []) => list(&root),
        ("info", [name]) => info(&root, name),
        ("install", [flag, files @ ..]) if flag == "--replace" && !files.is_empty() => {
            replace_files(&root, files)
        }
        ("install", files) if !files.is_empty() => install_files(&root, files),
        ("remove", [name]) => remove_one(&root, name),
        ("update", []) => update(&root, None),
        ("update", [flag, repo]) if flag == "--repo" => update(&root, Some(repo)),
        ("newer", []) => newer(&root),
        ("upgrade", [flag, repo, names @ ..]) if flag == "--repo" => {
            upgrade(&root, Some(repo), names)
        }
        ("upgrade", names) if !names.first().is_some_and(|n| n.starts_with("--")) => {
            upgrade(&root, None, names)
        }
        _ => return usage(),
    };
    match done {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "pkg: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> ExitCode {
    let _ = writeln!(std::io::stderr(), "{USAGE}");
    ExitCode::from(2)
}

/// Each line, to standard output.
fn say(lines: &[String]) -> Result<(), Error> {
    let mut out = std::io::stdout().lock();
    for line in lines {
        writeln!(out, "{line}").map_err(|error| Error(format!("standard output: {error}")))?;
    }
    Ok(())
}

fn list(root: &std::path::Path) -> Result<(), Error> {
    let lines: Vec<String> = installed(root)?
        .iter()
        .map(|record| {
            let package = &record.package;
            format!(
                "{:<16} {:<10} {}",
                package.name,
                package.version.as_str(),
                package.description
            )
        })
        .collect();
    say(&lines)
}

fn info(root: &std::path::Path, name: &str) -> Result<(), Error> {
    let record = installed(root)?
        .into_iter()
        .find(|record| record.package.name == name)
        .ok_or_else(|| Error(format!("{name} is not installed")))?;
    say(&info_lines(&record))
}

/// Each file read as a package.
fn read_files(files: &[String]) -> Result<Vec<pkg::Package>, Error> {
    let mut packages = Vec::new();
    for file in files {
        let bytes = std::fs::read(file).map_err(|error| Error(format!("{file}: {error}")))?;
        packages.push(read(&bytes).map_err(|error| Error(format!("{file}: {error}")))?);
    }
    Ok(packages)
}

fn replace_files(root: &std::path::Path, files: &[String]) -> Result<(), Error> {
    say(&replaced_lines(&install_replacing(
        root,
        read_files(files)?,
    )?))
}

/// A line for each package put in place, naming the version it replaced.
fn replaced_lines(
    done: &[(
        ferrix_pkg::record::Record,
        Option<ferrix_pkg::record::Record>,
    )],
) -> Vec<String> {
    done.iter()
        .map(|(record, old)| {
            let package = &record.package;
            match old {
                Some(old) => format!(
                    "pkg: replaced {} {} with {} ({} files)",
                    package.name,
                    old.package.version.as_str(),
                    package.version.as_str(),
                    record.files.len()
                ),
                None => format!(
                    "pkg: installed {} {} ({} files)",
                    package.name,
                    package.version.as_str(),
                    record.files.len()
                ),
            }
        })
        .collect()
}

fn update(root: &std::path::Path, given: Option<&str>) -> Result<(), Error> {
    let repository = repo::repository(root, given)?;
    let line = match repo::update(root, &repository)? {
        Updated::Same(index) => format!(
            "pkg: index {} for {} is the one accepted already",
            index.sequence, index.arch
        ),
        Updated::Accepted(index, before) => format!(
            "pkg: accepted index {} for {} from {}: {} package version(s){}",
            index.sequence,
            index.arch,
            repository.display(),
            index.entries.len(),
            before.map_or_else(String::new, |old| format!(", in the place of index {old}"))
        ),
    };
    say(&[line])
}

fn newer(root: &std::path::Path) -> Result<(), Error> {
    say(&repo::newer_lines(root)?)
}

fn upgrade(root: &std::path::Path, given: Option<&str>, names: &[String]) -> Result<(), Error> {
    let repository = repo::repository(root, given)?;
    let done = repo::upgrade(root, &repository, names)?;
    if done.is_empty() {
        return say(&["pkg: nothing to upgrade".to_owned()]);
    }
    say(&replaced_lines(&done))
}

fn install_files(root: &std::path::Path, files: &[String]) -> Result<(), Error> {
    let lines: Vec<String> = install(root, read_files(files)?)?
        .iter()
        .map(|record| {
            format!(
                "pkg: installed {} {} ({} files)",
                record.package.name,
                record.package.version.as_str(),
                record.files.len()
            )
        })
        .collect();
    say(&lines)
}

fn remove_one(root: &std::path::Path, name: &str) -> Result<(), Error> {
    let (record, gone) = remove(root, name)?;
    let mut lines = vec![format!(
        "pkg: removed {} {} ({} files)",
        record.package.name,
        record.package.version.as_str(),
        record.files.len()
    )];
    lines.extend(
        gone.iter()
            .map(|path| format!("pkg: /{path} was already gone")),
    );
    say(&lines)
}
