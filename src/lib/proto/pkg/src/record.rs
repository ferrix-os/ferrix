//! The record an installed package leaves: `lib/ferrix/packages/<name>.toml`.
//!
//! The package's `[package]`, built for one architecture, and a
//! `[[files]]` entry for each file it put down with the file's mode, size
//! and BLAKE2b-256 digest; a symbolic link has its target as `link`, and its
//! size and digest are the target's. It is inside the package, beside the files, so
//! installing -- which is unpacking -- puts it down too: a running Ferrix
//! knows what its image was built with, and what to delete to remove one.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use ferrix_argon2::blake2b;

use crate::manifest::{self, Error, Package, refuse};
use crate::toml::{self, Table};

/// Where records are, from the root.
pub const RECORDS: &str = "lib/ferrix/packages";

/// The bytes of a digest.
pub const DIGEST: usize = 32;

/// The path of `name`'s record, from the root.
#[must_use]
pub fn path(name: &str) -> String {
    format!("{RECORDS}/{name}.toml")
}

/// A file a package put down: a regular file, or a symbolic link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// Its path from the root.
    pub path: String,
    /// Its permission bits.
    pub mode: u32,
    /// Its length in bytes: a link's, its target's.
    pub size: u64,
    /// Its BLAKE2b-256 digest: a link's, its target's.
    pub digest: [u8; DIGEST],
    /// A symbolic link's target, as written.
    pub link: Option<String>,
}

impl Installed {
    /// The entry for `bytes` at `path` with `mode`.
    #[must_use]
    pub fn of(path: &str, mode: u32, bytes: &[u8]) -> Self {
        let mut digest = [0; DIGEST];
        blake2b::digest(bytes, &mut digest);
        Self {
            path: path.to_owned(),
            mode,
            size: bytes.len() as u64,
            digest,
            link: None,
        }
    }

    /// The entry for a symbolic link at `path` to `target`.
    #[must_use]
    pub fn link(path: &str, target: &str) -> Self {
        Self {
            link: Some(target.to_owned()),
            ..Self::of(path, LINK_MODE, target.as_bytes())
        }
    }
}

/// A symbolic link's permission bits, which nothing reads.
pub const LINK_MODE: u32 = 0o777;

/// An installed package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// What it is. `arches` is the one architecture it was built for.
    pub package: Package,
    /// What it put down, the record itself not among them.
    pub files: Vec<Installed>,
}

impl Record {
    /// Every path the package owns: its files and its record.
    pub fn paths(&self) -> impl Iterator<Item = String> + '_ {
        self.files
            .iter()
            .map(|file| file.path.clone())
            .chain(core::iter::once(path(&self.package.name)))
    }
}

/// `record` as its file.
#[must_use]
pub fn render(record: &Record) -> String {
    let mut out = String::new();
    // Writing to a `String` does not fail.
    let _ = write_record(&mut out, record);
    out
}

fn write_record(out: &mut String, record: &Record) -> fmt::Result {
    let package = &record.package;
    out.write_str("[package]\nname = ")?;
    toml::write_string(out, &package.name)?;
    out.write_str("\nversion = ")?;
    toml::write_string(out, package.version.as_str())?;
    out.write_str("\ndescription = ")?;
    toml::write_string(out, &package.description)?;
    out.write_str("\nlicense = ")?;
    toml::write_string(out, &package.license)?;
    out.write_str("\nabi = ")?;
    toml::write_string(out, package.abi.as_str())?;
    out.write_str("\narches = ")?;
    toml::write_array(out, &package.arches)?;
    out.write_str("\ndepends = ")?;
    let depends: Vec<String> = package.depends.iter().map(|d| format!("{d}")).collect();
    toml::write_array(out, &depends)?;
    out.write_char('\n')?;
    for file in &record.files {
        out.write_str("\n[[files]]\npath = ")?;
        toml::write_string(out, &file.path)?;
        if let Some(target) = &file.link {
            out.write_str("\nlink = ")?;
            toml::write_string(out, target)?;
        }
        write!(
            out,
            "\nmode = \"{:o}\"\nsize = \"{}\"\nblake2b = \"",
            file.mode, file.size
        )?;
        for byte in file.digest {
            write!(out, "{byte:02x}")?;
        }
        out.write_str("\"\n")?;
    }
    Ok(())
}

/// Read a record.
///
/// # Errors
///
/// Text that is not the subset, a table or key a record does not have, and
/// a field that is not what a record writes.
pub fn parse(text: &str) -> Result<Record, Error> {
    let document = toml::parse(text)?;
    for table in &document.tables {
        let known = match (table.name.as_str(), table.array) {
            ("", false) => table.entries.is_empty(),
            ("package", false) | ("files", true) => true,
            _ => false,
        };
        if !known {
            return refuse(format!(
                "line {}: `{}` is not in a record",
                table.line, table.name
            ));
        }
    }
    let package = document
        .table("package")
        .ok_or_else(|| Error("a record has no [package]".to_owned()))?;
    let package = manifest::read_package(package)?;
    if package.arches.len() != 1 {
        return refuse("a record's arches is the one it was built for".to_owned());
    }
    let files = document
        .array("files")
        .map(read_installed)
        .collect::<Result<Vec<_>, _>>()?;
    manifest::no_path_twice(files.iter().map(|file| file.path.as_str()))?;
    Ok(Record { package, files })
}

fn read_installed(table: &Table) -> Result<Installed, Error> {
    manifest::known_keys(table, &["path", "link", "mode", "size", "blake2b"])?;
    let path = manifest::string(table, "path")?;
    if !manifest::is_safe_path(&path) {
        return refuse(format!("[[files]] path: `{path}` is not under the root"));
    }
    let size = manifest::string(table, "size")?;
    let size = size
        .parse()
        .map_err(|_| Error(format!("[[files]] size: `{size}` is not a number")))?;
    let hex = manifest::string(table, "blake2b")?;
    let digest = digest(&hex).ok_or_else(|| {
        Error(format!(
            "[[files]] blake2b: `{hex}` is not {DIGEST} bytes in hex"
        ))
    })?;
    let link = match table.get("link") {
        None => None,
        Some(toml::Value::String(target)) if !target.is_empty() => Some(target.clone()),
        Some(_) => return refuse(format!("[[files]] link: `{path}`'s target is not a path")),
    };
    Ok(Installed {
        path,
        mode: manifest::mode(&manifest::string(table, "mode")?)?,
        size,
        digest,
        link,
    })
}

/// A digest from lower-case hex.
fn digest(hex: &str) -> Option<[u8; DIGEST]> {
    let mut out = [0; DIGEST];
    if hex.len() != DIGEST * 2 {
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
