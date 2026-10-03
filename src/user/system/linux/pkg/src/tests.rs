#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "tests"
)]

use std::fs;
use std::path::{Path, PathBuf};

use ferrix_pkg::manifest;
use ferrix_pkg::record::{self, Installed, Record};

use super::{Package, info_lines, install, installed, read, remove, this_arch};

/// What one entry of a test package is.
enum Made {
    File(&'static [u8]),
    Link(&'static str),
}

/// A newc archive of `entries`, each `(name, mode with type, data)`.
fn newc(entries: &[(String, u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut put = |name: &str, mode: u32, data: &[u8], ino: u32| {
        let header = format!(
            "070701{ino:08x}{mode:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}{:08x}",
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            0,
            0,
            name.len() + 1,
            0
        );
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    };
    for (at, (name, mode, data)) in entries.iter().enumerate() {
        put(name, *mode, data, u32::try_from(at).unwrap() + 1);
    }
    put("TRAILER!!!", 0, &[], 0);
    out
}

/// The record of `name`, depending on `depends`, built for `arch`.
fn record_of(name: &str, depends: &[&str], arch: &str, files: Vec<Installed>) -> Record {
    let depends: Vec<String> = depends.iter().map(|d| format!("\"{d}\"")).collect();
    let text = format!(
        "[package]\nname = \"{name}\"\nversion = \"1.0\"\ndescription = \"test {name}\"\nlicense = \"MIT\"\n\
         abi = \"linux\"\narches = [\"{arch}\"]\ndepends = [{}]\n\n\
         [[package.files]]\nfrom = \"x\"\nto = \"bin/x\"\nmode = \"755\"\n",
        depends.join(", ")
    );
    let mut package = manifest::recipe(&text).unwrap().package;
    package.arches = vec![arch.to_owned()];
    Record { package, files }
}

/// A package of `name` holding `files`, its record saying `listed` of them
/// (all of them when `None`).
fn package(
    name: &str,
    depends: &[&str],
    files: &[(&str, Made)],
    arch: &str,
    tamper: Option<&str>,
) -> Vec<u8> {
    let mut entries = Vec::new();
    let mut listed = Vec::new();
    for (path, made) in files {
        match made {
            Made::File(bytes) => {
                listed.push(Installed::of(path, 0o755, bytes));
                let mut data = bytes.to_vec();
                if tamper == Some(*path) {
                    data.push(b'!');
                }
                entries.push(((*path).to_owned(), 0o100_755, data));
            }
            Made::Link(target) => {
                listed.push(Installed::link(path, target));
                entries.push(((*path).to_owned(), 0o120_777, target.as_bytes().to_vec()));
            }
        }
    }
    let record = record_of(name, depends, arch, listed);
    entries.push((
        record::path(name),
        0o100_644,
        record::render(&record).into_bytes(),
    ));
    newc(&entries)
}

/// An empty root of its own for `test`.
fn root(test: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pkg-{test}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn names(root: &Path) -> Vec<String> {
    installed(root)
        .unwrap()
        .into_iter()
        .map(|record| record.package.name)
        .collect()
}

fn base() -> Vec<u8> {
    package(
        "base",
        &[],
        &[
            ("usr/lib/base/data", Made::File(b"base data")),
            ("bin/base", Made::File(b"#!/bin/sh\necho base\n")),
            ("bin/base-too", Made::Link("base")),
        ],
        this_arch(),
        None,
    )
}

fn user() -> Vec<u8> {
    package(
        "user",
        &["base"],
        &[("bin/user", Made::File(b"#!/bin/sh\necho user\n"))],
        this_arch(),
        None,
    )
}

fn read_all(archives: &[Vec<u8>]) -> Vec<Package> {
    archives.iter().map(|bytes| read(bytes).unwrap()).collect()
}

#[test]
fn a_package_goes_in_and_comes_out_whole() {
    let root = root("whole");
    let done = install(&root, read_all(&[base()])).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(names(&root), ["base"]);
    assert_eq!(
        fs::read(root.join("usr/lib/base/data")).unwrap(),
        b"base data"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join("bin/base"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            fs::read_link(root.join("bin/base-too")).unwrap(),
            Path::new("base")
        );
    }
    let info = info_lines(&installed(&root).unwrap()[0]);
    assert!(info.contains(&"name: base".to_owned()), "{info:?}");
    assert!(
        info.contains(&"  /bin/base-too -> base".to_owned()),
        "{info:?}"
    );

    let (record, gone) = remove(&root, "base").unwrap();
    assert_eq!(record.package.name, "base");
    assert!(gone.is_empty(), "{gone:?}");
    assert!(names(&root).is_empty());
    assert!(!root.join("bin/base").exists());
    assert!(
        !root.join("usr/lib/base").exists(),
        "an emptied directory goes too"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_file_unlike_its_record_is_refused() {
    let tampered = package(
        "base",
        &[],
        &[("bin/base", Made::File(b"#!/bin/sh\n"))],
        this_arch(),
        Some("bin/base"),
    );
    let error = read(&tampered).unwrap_err();
    assert!(error.0.contains("not what its record says"), "{error}");
}

#[test]
fn a_package_for_another_machine_is_refused() {
    let other = if this_arch() == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    let foreign = package("base", &[], &[("bin/base", Made::File(b"x"))], other, None);
    let error = read(&foreign).unwrap_err();
    assert!(error.0.contains("this machine is"), "{error}");
}

#[test]
fn dependencies_come_first_or_together() {
    let root = root("depends");
    let error = install(&root, read_all(&[user()])).unwrap_err();
    assert!(error.0.contains("needs `base`"), "{error}");
    assert!(names(&root).is_empty(), "nothing went in");
    assert!(!root.join("bin/user").exists());

    // Together, in either order: the dependency goes in first.
    let done = install(&root, read_all(&[user(), base()])).unwrap();
    let order: Vec<&str> = done.iter().map(|r| r.package.name.as_str()).collect();
    assert_eq!(order, ["base", "user"]);

    let error = remove(&root, "base").unwrap_err();
    assert!(error.0.contains("user depends on base"), "{error}");
    assert!(root.join("bin/base").exists(), "nothing was removed");
    let _ = remove(&root, "user").unwrap();
    let _ = remove(&root, "base").unwrap();
    assert!(names(&root).is_empty());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn nothing_is_put_down_over_what_is_there() {
    let root = root("over");
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(root.join("bin/base"), b"the system's").unwrap();
    let error = install(&root, read_all(&[base()])).unwrap_err();
    assert!(
        error.0.contains("/bin/base, which is there already"),
        "{error}"
    );
    assert_eq!(fs::read(root.join("bin/base")).unwrap(), b"the system's");
    assert!(!root.join("usr/lib/base/data").exists(), "nothing went in");
    fs::remove_file(root.join("bin/base")).unwrap();

    let _ = install(&root, read_all(&[base()])).unwrap();
    let error = install(&root, read_all(&[base()])).unwrap_err();
    assert!(error.0.contains("installed already"), "{error}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_file_already_gone_is_said_and_the_rest_removed() {
    let root = root("gone");
    let _ = install(&root, read_all(&[base()])).unwrap();
    fs::remove_file(root.join("usr/lib/base/data")).unwrap();
    let (_, gone) = remove(&root, "base").unwrap();
    assert_eq!(gone, ["usr/lib/base/data"]);
    assert!(!root.join("bin/base").exists());
    let error = remove(&root, "base").unwrap_err();
    assert!(error.0.contains("is not installed"), "{error}");
    let _ = fs::remove_dir_all(&root);
}
