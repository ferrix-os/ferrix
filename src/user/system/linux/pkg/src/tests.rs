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

use super::{Package, info_lines, install, install_replacing, installed, read, remove, this_arch};

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
fn record_of(
    name: &str,
    version: &str,
    depends: &[&str],
    arch: &str,
    files: Vec<Installed>,
) -> Record {
    let depends: Vec<String> = depends.iter().map(|d| format!("\"{d}\"")).collect();
    let text = format!(
        "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"test {name}\"\nlicense = \"MIT\"\n\
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
    package_at(name, "1.0", depends, files, arch, tamper)
}

/// [`package`], of `version`.
fn package_at(
    name: &str,
    version: &str,
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
    let record = record_of(name, version, depends, arch, listed);
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

/// `base` 2.0: its program changed, its data gone, a file of its own added.
fn base_two(tamper: Option<&str>) -> Vec<u8> {
    package_at(
        "base",
        "2.0",
        &[],
        &[
            ("bin/base", Made::File(b"#!/bin/sh\necho base two\n")),
            ("bin/base-too", Made::Link("base")),
            ("usr/lib/base/two", Made::File(b"new in two")),
        ],
        this_arch(),
        tamper,
    )
}

#[test]
fn a_replacement_takes_the_old_version_place_whole() {
    let root = root("replace");
    let _ = install(&root, read_all(&[base(), user()])).unwrap();
    let error = install(&root, read_all(&[base_two(None)])).unwrap_err();
    assert!(error.0.contains("installed already"), "{error}");
    assert!(error.0.contains("--replace"), "{error}");

    let done = install_replacing(&root, read_all(&[base_two(None)])).unwrap();
    assert_eq!(done.len(), 1);
    let (new, old) = &done[0];
    assert_eq!(new.package.version.as_str(), "2.0");
    assert_eq!(old.as_ref().unwrap().package.version.as_str(), "1.0");
    assert_eq!(
        fs::read(root.join("bin/base")).unwrap(),
        b"#!/bin/sh\necho base two\n"
    );
    assert_eq!(
        fs::read(root.join("usr/lib/base/two")).unwrap(),
        b"new in two"
    );
    assert!(
        !root.join("usr/lib/base/data").exists(),
        "a file only the old version had goes"
    );
    let records = installed(&root).unwrap();
    let record = records.iter().find(|r| r.package.name == "base").unwrap();
    assert_eq!(record.package.version.as_str(), "2.0");
    assert_eq!(names(&root), ["base", "user"], "what depends on it stays");
    let leftovers: Vec<_> = walk(&root)
        .into_iter()
        .filter(|path| path.to_string_lossy().ends_with(".pkg-new"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");

    // And back: the newer version's own file goes with it.
    let _ = install_replacing(&root, read_all(&[base()])).unwrap();
    assert!(!root.join("usr/lib/base/two").exists());
    assert_eq!(
        fs::read(root.join("usr/lib/base/data")).unwrap(),
        b"base data"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_replacement_that_is_refused_changes_nothing() {
    let root = root("replace-refused");
    let _ = install(&root, read_all(&[base()])).unwrap();
    // Unlike its record: refused as it is read.
    assert!(read(&base_two(Some("bin/base"))).is_err());
    // A path of the system's, not the old version's: refused before a write.
    fs::create_dir_all(root.join("usr/lib/base")).unwrap();
    fs::write(root.join("usr/lib/base/two"), b"the system's").unwrap();
    let error = install_replacing(&root, read_all(&[base_two(None)])).unwrap_err();
    assert!(
        error
            .0
            .contains("/usr/lib/base/two, which is there already"),
        "{error}"
    );
    assert_eq!(
        fs::read(root.join("bin/base")).unwrap(),
        b"#!/bin/sh\necho base\n"
    );
    assert_eq!(
        fs::read(root.join("usr/lib/base/two")).unwrap(),
        b"the system's"
    );
    assert_eq!(installed(&root).unwrap()[0].package.version.as_str(), "1.0");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn replacing_what_is_not_installed_installs_it() {
    let root = root("replace-depends");
    let _ = install(&root, read_all(&[base(), user()])).unwrap();
    let other = package(
        "other",
        &[],
        &[("bin/other", Made::File(b"x"))],
        this_arch(),
        None,
    );
    // Replacing nothing is an install.
    let done = install_replacing(&root, read_all(&[other])).unwrap();
    assert!(done[0].1.is_none());
    assert_eq!(names(&root), ["base", "other", "user"]);
    let _ = fs::remove_dir_all(&root);
}

/// Every file under `root`.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if fs::symlink_metadata(&path).unwrap().is_dir() {
                todo.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}

mod repository {
    use std::fs;
    use std::path::{Path, PathBuf};

    use std::sync::OnceLock;

    use ferrix_pkg::index::{self, Index};
    use ferrix_sigverify::{Anchor, Kind};
    use ferrix_softsign::lms::LMS_SHA256_M24_H10;
    use ferrix_softsign::manifest::{self, Body};
    use ferrix_softsign::token::{CKA_VALUE, ObjectHandle, SoftToken, Token};

    use super::{Made, install, names, package_at, read, read_all, root};
    use crate::repo::{self, ACCEPTED, INDEX, Updated};
    use crate::{installed, sig, this_arch};

    /// The anchor id of the tests' package keys.
    const ANCHOR: u16 = 0x8f00;

    /// A development token holding two H10 package keys, `good` and
    /// `other`, made once per run outside any git tree; and `good`'s anchor.
    struct Keys {
        dir: PathBuf,
        anchor: Anchor,
    }

    fn keys() -> &'static Keys {
        static KEYS: OnceLock<Keys> = OnceLock::new();
        KEYS.get_or_init(|| {
            let dir = std::env::temp_dir().join(format!("pkg-test-keys-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            let mut token = SoftToken::open(&dir, Vec::new()).unwrap();
            let good = token
                .generate_from("good", LMS_SHA256_M24_H10, [5; 16], [6; 24])
                .unwrap();
            let _ = token
                .generate_from("other", LMS_SHA256_M24_H10, [7; 16], [8; 24])
                .unwrap();
            let public = token.get_attribute(&good, CKA_VALUE).unwrap();
            Keys {
                dir,
                anchor: Anchor {
                    kind: Kind::Package,
                    id: ANCHOR,
                    public_key: public.try_into().unwrap(),
                },
            }
        })
    }

    fn anchors() -> [Anchor; 1] {
        [keys().anchor]
    }

    /// The manifest of `payload` as `subject`'s, of `security_version`,
    /// signed by the key `label`.
    fn signed(label: &str, subject: &str, security_version: u32, payload: &[u8]) -> Vec<u8> {
        let mut token = SoftToken::open(&keys().dir, Vec::new()).unwrap();
        let mut padded = [0u8; 32];
        padded[..subject.len()].copy_from_slice(subject.as_bytes());
        let body = Body {
            kind: 5,
            security_version,
            anchor_id: ANCHOR,
            profile: 0xFF,
            arch: sig::arch_byte(),
            family: sig::family(),
            subject: padded,
            payload_len: payload.len() as u64,
            payload_sha256: index::sha256(payload),
            ..Body::default()
        };
        manifest::sign(&mut token, &ObjectHandle(label.into()), &body).unwrap()
    }

    /// Sign `file` in `dir`'s directory for this architecture as
    /// `subject`'s, with the key `label`.
    fn sign_file(dir: &Path, file: &str, label: &str, subject: &str, version: u32) {
        let at = dir.join(this_arch());
        let bytes = fs::read(at.join(file)).unwrap();
        fs::write(
            at.join(format!("{file}{}", sig::SUFFIX)),
            signed(label, subject, version, &bytes),
        )
        .unwrap();
    }

    /// A package of `name` and `version` whose one program says so.
    fn made(name: &str, version: &str, depends: &[&str]) -> Vec<u8> {
        let says: &'static [u8] = Box::leak(format!("{name} {version}\n").into_bytes().into());
        let program: &'static str = Box::leak(format!("bin/{name}").into_boxed_str());
        package_at(
            name,
            version,
            depends,
            &[(program, Made::File(says))],
            this_arch(),
            None,
        )
    }

    /// A repository at `dir` holding `packages`, its index of `sequence`
    /// with `security` marking those versions, by `name version`. Returns
    /// the index's bytes.
    fn publish(dir: &Path, sequence: u64, packages: &[Vec<u8>], security: &[&str]) -> Vec<u8> {
        let arch_dir = dir.join(this_arch());
        fs::create_dir_all(&arch_dir).unwrap();
        let mut entries = Vec::new();
        for bytes in packages {
            let record = read(bytes).unwrap().record;
            let mut entry = index::entry_of(&record, bytes);
            let named = format!("{} {}", entry.name, entry.version);
            if security.contains(&named.as_str()) {
                entry.security = true;
                entry.advisory = Some(format!("FXSA-2026-9: {named} fixes a fault"));
            }
            fs::write(arch_dir.join(&entry.file), bytes).unwrap();
            sign_file(dir, &entry.file, "good", &entry.name, 0);
            entries.push(entry);
        }
        let text = index::render(&Index {
            arch: this_arch().to_owned(),
            sequence,
            entries,
        });
        fs::write(arch_dir.join(INDEX), &text).unwrap();
        sign_file(
            dir,
            INDEX,
            "good",
            sig::INDEX_SUBJECT,
            u32::try_from(sequence).unwrap(),
        );
        text.into_bytes()
    }

    fn file_of(dir: &Path, name: &str, version: &str) -> PathBuf {
        dir.join(this_arch())
            .join(format!("{name}-{version}-{}.fxpkg", this_arch()))
    }

    fn version_of(root: &Path, name: &str) -> String {
        installed(root)
            .unwrap()
            .into_iter()
            .find(|r| r.package.name == name)
            .map(|r| r.package.version.as_str().to_owned())
            .unwrap_or_default()
    }

    fn refused<T: std::fmt::Debug>(result: Result<T, crate::Error>, why: &str) {
        let error = result.expect_err(why);
        assert!(error.0.contains(why), "{error} does not say {why:?}");
    }

    #[test]
    fn an_upgrade_from_a_repository_brings_the_security_fix() {
        let root = root("repo-upgrade");
        let repository = root.join("srv");
        let _ = install(&root, read_all(&[made("base", "1.0", &[])])).unwrap();
        let _ = install(&root, read_all(&[made("app", "1.0", &["base"])])).unwrap();
        refused(
            repo::newer_lines(&root),
            "no index accepted yet: run pkg update",
        );
        let _ = publish(
            &repository,
            1,
            &[
                made("base", "1.0", &[]),
                made("base", "1.1", &[]),
                made("base", "1.2", &[]),
                made("app", "2.0", &["base >= 1.2", "lib"]),
                made("lib", "1.0", &[]),
            ],
            &["base 1.1"],
        );
        let updated = repo::update(&root, &repository, &anchors()).unwrap();
        assert!(matches!(updated, Updated::Accepted(ref i, None) if i.sequence == 1));
        assert!(fs::read(root.join(ACCEPTED)).is_ok());
        assert!(matches!(
            repo::update(&root, &repository, &anchors()).unwrap(),
            Updated::Same(_)
        ));
        let lines = repo::newer_lines(&root).unwrap();
        assert_eq!(
            lines,
            [
                "app 1.0 -> 2.0",
                "base 1.0 -> 1.2  (security)",
                "  security fix in 1.1: FXSA-2026-9: base 1.1 fixes a fault",
                "pkg: 2 package(s) have a newer version, 1 with a security fix (index 1)",
            ]
        );
        // base alone, by name.
        let done = repo::upgrade(&root, &repository, &["base".to_owned()], &anchors()).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(version_of(&root, "base"), "1.2");
        assert_eq!(fs::read(root.join("bin/base")).unwrap(), b"base 1.2\n");
        // The rest: app 2.0, and lib, which it needs and was not installed.
        let done = repo::upgrade(&root, &repository, &[], &anchors()).unwrap();
        let went: Vec<String> = done
            .iter()
            .map(|(r, old)| format!("{} {}", r.package.name, old.is_some()))
            .collect();
        assert_eq!(went, ["lib false", "app true"]);
        assert_eq!(names(&root), ["app", "base", "lib"]);
        assert!(
            repo::upgrade(&root, &repository, &[], &anchors())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repo::newer_lines(&root).unwrap(),
            ["pkg: every package is the newest of index 1"]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_index_older_than_the_accepted_one_is_refused() {
        let root = root("repo-older");
        let repository = root.join("srv");
        let base = made("base", "1.0", &[]);
        let first = publish(&repository, 5, std::slice::from_ref(&base), &[]);
        assert!(matches!(
            repo::update(&root, &repository, &anchors()).unwrap(),
            Updated::Accepted(_, None)
        ));
        // Older: refused, and the accepted index stays.
        let _ = publish(&repository, 4, std::slice::from_ref(&base), &[]);
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index: it is older than the one accepted (BelowFloor): refused",
        );
        assert_eq!(fs::read(root.join(ACCEPTED)).unwrap(), first);
        // The same sequence, other contents: refused.
        let _ = publish(
            &repository,
            5,
            &[base.clone(), made("lib", "1.0", &[])],
            &[],
        );
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index 5 is not the index 5 accepted before: refused",
        );
        // Newer: accepted, naming the one it replaced.
        let _ = publish(&repository, 6, &[base], &[]);
        assert!(matches!(
            repo::update(&root, &repository, &anchors()).unwrap(),
            Updated::Accepted(ref i, Some(5)) if i.sequence == 6
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_unlike_the_index_is_refused_before_anything_is_written() {
        let root = root("repo-digest");
        let repository = root.join("srv");
        let _ = install(&root, read_all(&[made("base", "1.0", &[])])).unwrap();
        let _ = publish(&repository, 1, &[made("base", "2.0", &[])], &[]);
        let _ = repo::update(&root, &repository, &anchors()).unwrap();
        let file = file_of(&repository, "base", "2.0");
        let good = fs::read(&file).unwrap();
        // One byte changed, the length kept: the digest refuses it.
        let mut changed = good.clone();
        let last = changed.len() - 1;
        changed[last] ^= 1;
        fs::write(&file, &changed).unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "base 2.0: base-2.0-",
        );
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "is not the file the index names (its SHA-256 differs): refused",
        );
        // A byte more: the size refuses it.
        let mut longer = good.clone();
        longer.push(0);
        fs::write(&file, &longer).unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            &format!("bytes, and the index says {}", good.len()),
        );
        // Another package in its place, with the index made for it: the
        // digest is right, the package is not the one the index names.
        let other = made("base", "3.0", &[]);
        let text = fs::read_to_string(repository.join(this_arch()).join(INDEX)).unwrap();
        let text = text
            .replace(
                &index::hex(&index::sha256(&good)),
                &index::hex(&index::sha256(&other)),
            )
            .replace(
                &format!("size = \"{}\"", good.len()),
                &format!("size = \"{}\"", other.len()),
            )
            .replace("sequence = \"1\"", "sequence = \"2\"");
        fs::write(repository.join(this_arch()).join(INDEX), text).unwrap();
        sign_file(&repository, INDEX, "good", sig::INDEX_SUBJECT, 2);
        fs::write(&file, &other).unwrap();
        sign_file(
            &repository,
            &format!("base-2.0-{}.fxpkg", this_arch()),
            "good",
            "base",
            0,
        );
        let _ = repo::update(&root, &repository, &anchors()).unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "holds base 3.0, which is not the package the index names: refused",
        );
        // Nothing of any of them went in.
        assert_eq!(version_of(&root, "base"), "1.0");
        assert_eq!(fs::read(root.join("bin/base")).unwrap(), b"base 1.0\n");
        // The control: the right file goes in.
        fs::write(&file, &good).unwrap();
        let _ = publish(&repository, 3, &[made("base", "2.0", &[])], &[]);
        let _ = repo::update(&root, &repository, &anchors()).unwrap();
        assert_eq!(
            repo::upgrade(&root, &repository, &[], &anchors())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(version_of(&root, "base"), "2.0");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_dependency_nobody_has_is_refused() {
        let root = root("repo-missing");
        let repository = root.join("srv");
        let _ = install(&root, read_all(&[made("app", "1.0", &[])])).unwrap();
        let _ = publish(&repository, 1, &[made("app", "2.0", &["gone"])], &[]);
        let _ = repo::update(&root, &repository, &anchors()).unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "app 2.0 needs gone, which neither this root nor the repository has",
        );
        refused(
            repo::upgrade(&root, &repository, &["other".to_owned()], &anchors()),
            "other is not installed",
        );
        assert_eq!(version_of(&root, "app"), "1.0");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_repository_is_named_or_configured() {
        let root = root("repo-named");
        refused(
            repo::repository(&root, None),
            "no repository: name one with --repo",
        );
        refused(
            repo::repository(&root, Some("https://example.org/ferrix")),
            "fetching over HTTP is not built yet",
        );
        assert_eq!(
            repo::repository(&root, Some("file:///srv/ferrix")).unwrap(),
            PathBuf::from("/srv/ferrix")
        );
        fs::create_dir_all(root.join("etc/pkg")).unwrap();
        fs::write(root.join(repo::REPOSITORY), "/srv/pkgs\n# a comment line\n").unwrap();
        assert_eq!(
            repo::repository(&root, None).unwrap(),
            PathBuf::from("/srv/pkgs")
        );
        // An index for another architecture.
        let repository = root.join("srv");
        fs::create_dir_all(repository.join(this_arch())).unwrap();
        let other = if this_arch() == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        fs::write(
            repository.join(this_arch()).join(INDEX),
            format!("[index]\narch = \"{other}\"\nsequence = \"1\"\n"),
        )
        .unwrap();
        sign_file(&repository, INDEX, "good", sig::INDEX_SUBJECT, 1);
        refused(
            repo::update(&root, &repository, &anchors()),
            &format!("the repository's the index is for {other}"),
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_signature_that_does_not_hold_is_refused() {
        let root = root("repo-signature");
        let repository = root.join("srv");
        let _ = install(&root, read_all(&[made("base", "1.0", &[])])).unwrap();
        let _ = publish(&repository, 2, &[made("base", "2.0", &[])], &[]);
        let base = format!("base-2.0-{}.fxpkg", this_arch());
        // The index signed by a key that is not the anchor's.
        sign_file(&repository, INDEX, "other", sig::INDEX_SUBJECT, 2);
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index: its signature does not verify (BadSignature): refused",
        );
        // Signed by the anchor's key, but this pkg trusts no key.
        sign_file(&repository, INDEX, "good", sig::INDEX_SUBJECT, 2);
        refused(
            repo::update(&root, &repository, &[]),
            "it is signed by no key this pkg trusts (UnknownAnchor): refused",
        );
        // Signed as a package, not the index.
        sign_file(&repository, INDEX, "good", "base", 2);
        refused(
            repo::update(&root, &repository, &anchors()),
            "(WrongSubject): refused",
        );
        // Signed with another sequence than the index says.
        sign_file(&repository, INDEX, "good", sig::INDEX_SUBJECT, 3);
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index says it is 2, and its signature names 3: refused",
        );
        // No signature.
        let at = repository.join(this_arch());
        fs::remove_file(at.join(format!("{INDEX}{}", sig::SUFFIX))).unwrap();
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index has no signature, index.toml.fxsig: refused",
        );
        assert!(fs::read(root.join(ACCEPTED)).is_err());
        // The control: signed as it should be, it is accepted.
        sign_file(&repository, INDEX, "good", sig::INDEX_SUBJECT, 2);
        assert!(matches!(
            repo::update(&root, &repository, &anchors()).unwrap(),
            Updated::Accepted(_, None)
        ));
        // A package signed by the other key, signed as another package,
        // and with no signature: each refused, nothing written.
        sign_file(&repository, &base, "other", "base", 0);
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "base 2.0: its signature does not verify (BadSignature): refused",
        );
        sign_file(&repository, &base, "good", "app", 0);
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "base 2.0: it is signed for something else (WrongSubject): refused",
        );
        fs::remove_file(at.join(format!("{base}{}", sig::SUFFIX))).unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            &format!("base 2.0 has no signature, {base}.fxsig: refused"),
        );
        assert_eq!(version_of(&root, "base"), "1.0");
        // The control: its own signature, and it goes in.
        sign_file(&repository, &base, "good", "base", 0);
        assert_eq!(
            repo::upgrade(&root, &repository, &[], &anchors())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(version_of(&root, "base"), "2.0");
        let _ = fs::remove_dir_all(&root);
    }

    /// The one check that ties a file's bytes to its signature,
    /// `check_payload` in [`sig::check`]: a good signature beside other
    /// bytes is refused, for the index and for a package.
    #[test]
    fn a_good_signature_beside_other_bytes_is_refused() {
        let root = root("repo-payload");
        let repository = root.join("srv");
        let at = repository.join(this_arch());
        let _ = install(&root, read_all(&[made("base", "1.0", &[])])).unwrap();
        let first = publish(&repository, 1, &[made("base", "1.0", &[])], &[]);
        let _ = repo::update(&root, &repository, &anchors()).unwrap();
        // Index 2, signed, then one byte of it changed: the length, the
        // sequence and the syntax kept, so only the digest can tell.
        let good = publish(&repository, 2, &[made("base", "2.0", &[])], &["base 2.0"]);
        let changed = String::from_utf8(good.clone())
            .unwrap()
            .replace("fixes a fault", "fixes a fauLt")
            .into_bytes();
        assert_eq!(changed.len(), good.len());
        assert_ne!(changed, good);
        fs::write(at.join(INDEX), &changed).unwrap();
        refused(
            repo::update(&root, &repository, &anchors()),
            "the repository's index: it is not the file its signature names (PayloadMismatch): refused",
        );
        assert_eq!(fs::read(root.join(ACCEPTED)).unwrap(), first);
        // The control: its own bytes, and it is accepted.
        fs::write(at.join(INDEX), &good).unwrap();
        assert!(matches!(
            repo::update(&root, &repository, &anchors()).unwrap(),
            Updated::Accepted(ref i, Some(1)) if i.sequence == 2
        ));
        // A package the index names, beside a signature under its own name
        // of other bytes of the same length: refused, nothing written.
        let base = format!("base-2.0-{}.fxpkg", this_arch());
        let mut other = fs::read(at.join(&base)).unwrap();
        let last = other.len() - 1;
        other[last] ^= 1;
        fs::write(
            at.join(format!("{base}{}", sig::SUFFIX)),
            signed("good", "base", 0, &other),
        )
        .unwrap();
        refused(
            repo::upgrade(&root, &repository, &[], &anchors()),
            "base 2.0: it is not the file its signature names (PayloadMismatch): refused",
        );
        assert_eq!(version_of(&root, "base"), "1.0");
        assert_eq!(fs::read(root.join("bin/base")).unwrap(), b"base 1.0\n");
        // The control: its own signature, and it goes in.
        sign_file(&repository, &base, "good", "base", 0);
        assert_eq!(
            repo::upgrade(&root, &repository, &[], &anchors())
                .unwrap()
                .len(),
            1
        );
        assert_eq!(version_of(&root, "base"), "2.0");
        let _ = fs::remove_dir_all(&root);
    }
}

mod transaction {
    use std::fs;
    use std::path::Path;

    use super::{
        Made, base, base_two, install, install_replacing, installed, package_at, read_all, root,
        user, walk,
    };
    use crate::transaction::{JOURNAL, OLD, Recovered, Stop, recover, run_until};
    use crate::{install_with, this_arch};

    /// user 2.0: its program changed.
    fn user_two() -> Vec<u8> {
        package_at(
            "user",
            "2.0",
            &["base"],
            &[("bin/user", Made::File(b"#!/bin/sh\necho user two\n"))],
            this_arch(),
            None,
        )
    }

    /// Every file and link under `root` with what it holds, sorted.
    fn snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
        let mut out: Vec<(String, Vec<u8>)> = walk(root)
            .into_iter()
            .map(|path| {
                let held = match fs::read_link(&path) {
                    Ok(target) => target.to_string_lossy().into_owned().into_bytes(),
                    Err(_) => fs::read(&path).unwrap(),
                };
                (path.to_string_lossy().into_owned(), held)
            })
            .collect();
        out.sort();
        out
    }

    fn versions(root: &Path) -> Vec<String> {
        installed(root)
            .unwrap()
            .iter()
            .map(|r| format!("{} {}", r.package.name, r.package.version))
            .collect()
    }

    #[test]
    fn a_failure_part_way_leaves_the_root_as_it_was() {
        // A rename that fails at each point of the swap: everything before
        // it is undone, and the root is as it was.
        for k in 0.. {
            let root = root(&format!("txn-fail-{k}"));
            let _ = install(&root, read_all(&[base(), user()])).unwrap();
            let before = snapshot(&root);
            let packages = read_all(&[base_two(None), user_two()]);
            match install_with(&root, packages, true, Some(Stop::Fail(k))) {
                Err(error) => {
                    assert!(
                        error.0.contains("every change undone, nothing changed"),
                        "{error}"
                    );
                    assert_eq!(snapshot(&root), before, "fail {k}");
                    let _ = fs::remove_dir_all(&root);
                }
                Ok(_) => {
                    assert!(k > 5, "the swap has more than five renames");
                    let _ = fs::remove_dir_all(&root);
                    break;
                }
            }
        }
    }

    #[test]
    fn something_in_the_way_is_refused_before_anything_changes() {
        let root = root("txn-in-the-way");
        let _ = install(&root, read_all(&[base(), user()])).unwrap();
        fs::create_dir_all(root.join("bin/user.pkg-old/in-the-way")).unwrap();
        let before = snapshot(&root);
        let error = install_replacing(&root, read_all(&[base_two(None), user_two()])).unwrap_err();
        assert!(
            error
                .0
                .contains("/bin/user.pkg-old is in the way of /bin/user being set aside"),
            "{error}"
        );
        assert_eq!(snapshot(&root), before);
        assert_eq!(versions(&root), ["base 1.0", "user 1.0"]);
        assert!(!root.join(JOURNAL).exists());
        // The control: out of the way, the same upgrade goes in whole.
        fs::remove_dir_all(root.join("bin/user.pkg-old")).unwrap();
        let _ = install_replacing(&root, read_all(&[base_two(None), user_two()])).unwrap();
        assert_eq!(versions(&root), ["base 2.0", "user 2.0"]);
        let leftovers: Vec<_> = walk(&root)
            .into_iter()
            .filter(|path| {
                let path = path.to_string_lossy();
                path.ends_with(OLD) || path.ends_with(".pkg-new")
            })
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert!(!root.join("usr/lib/base/data").exists());
        assert!(!root.join(JOURNAL).exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_stop_at_any_rename_is_undone_by_the_next_pkg() {
        // Every point of the swap: base 2.0 and user 2.0 replace 1.0 and
        // stop dead after `k` renames; recovery puts back exactly what was
        // there. Past the last rename the transaction completes.
        for k in 0.. {
            let root = root(&format!("txn-stop-{k}"));
            let _ = install(&root, read_all(&[base(), user()])).unwrap();
            let before = snapshot(&root);
            let packages = read_all(&[base_two(None), user_two()]);
            match install_with(&root, packages, true, Some(Stop::Crash(k))) {
                Err(_) => {
                    assert!(root.join(JOURNAL).exists(), "stop {k}: no journal");
                    // Nothing else changes the root until it is put right.
                    let again = run_until(&root, &[], None).unwrap_err();
                    assert!(again.0.contains("stopped part way"), "{again}");
                    assert_eq!(recover(&root).unwrap(), Some(Recovered::RolledBack));
                    assert_eq!(snapshot(&root), before, "stop {k}");
                    assert_eq!(recover(&root).unwrap(), None);
                    let _ = fs::remove_dir_all(&root);
                }
                Ok(_) => {
                    assert!(k > 5, "the swap has more than five renames");
                    assert_eq!(versions(&root), ["base 2.0", "user 2.0"]);
                    let _ = fs::remove_dir_all(&root);
                    break;
                }
            }
        }
    }

    #[test]
    fn a_stop_after_the_commit_is_finished_by_the_next_pkg() {
        let root = root("txn-committed");
        let _ = install(&root, read_all(&[base_two(None)])).unwrap();
        // As a stop after the commit leaves it: the new files in place, an
        // old one still set aside, the journal committed.
        fs::write(root.join("usr/lib/base/data.pkg-old"), b"base data").unwrap();
        fs::create_dir_all(root.join("var/lib/pkg")).unwrap();
        fs::write(
            root.join(JOURNAL),
            "pkg transaction 1\ncommitted\nd usr/lib/base/data\n",
        )
        .unwrap();
        assert_eq!(recover(&root).unwrap(), Some(Recovered::Finished));
        assert!(!root.join("usr/lib/base/data.pkg-old").exists());
        assert!(!root.join(JOURNAL).exists());
        assert_eq!(versions(&root), ["base 2.0"]);
        // A journal pkg did not write is refused, and touched nowhere.
        fs::write(
            root.join(JOURNAL),
            "pkg transaction 1\nswapping\nd ../etc\n",
        )
        .unwrap();
        let error = recover(&root).unwrap_err();
        assert!(error.0.contains("not a journal pkg wrote"), "{error}");
        assert!(root.join(JOURNAL).exists());
        let _ = fs::remove_dir_all(&root);
    }
}
