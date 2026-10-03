//! The reader against every form it takes and a sample of what it refuses;
//! a record through render and parse; the plan's refusals, each one.

extern crate std;

use alloc::borrow::ToOwned;
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{format, vec};

use crate::manifest::{self, Abi, Build, Dependency, Source, Version};
use crate::plan::plan;
use crate::record::{self, Installed, Record};
use crate::toml::{self, Value};

/// An app's manifest with every table.
const APP: &str = r#"
# A comment, and one after a value.
[package]
name = "ferrofetch"            # the folder's name
version = "0.1.0"
description = "The mark, and a few lines about the machine."
license = "MIT"
abi = "native"
arches = ["x86_64", "aarch64", "armv7a",]
depends = ["zlib >= 1.3", "curl"]

[[package.files]]
from = "ferrofetch"
to = "bin/ferrofetch"
mode = "755"

[[package.files]]
from = "share/ferrofetch.txt"
source = "folder"
to = "usr/share/ferrofetch/note.txt"
mode = "644"

[build]
kind = "cargo"

[image]
default = false

[check]
host-tests = true

[[smoke]]
run = "ferrofetch --no-logo --no-color"
expect = "OS: Ferrix"

[[smoke]]
run = "ferrofetch --version"
expect = "ferrofetch \"0.1\""
"#;

#[test]
fn a_manifest_with_every_table_reads() {
    let recipe = manifest::recipe(APP).expect("it reads");
    let package = &recipe.package;
    assert_eq!(package.name, "ferrofetch");
    assert_eq!(package.version.as_str(), "0.1.0");
    assert_eq!(package.abi, Abi::Native);
    assert_eq!(package.arches, ["x86_64", "aarch64", "armv7a"]);
    assert_eq!(
        package.depends,
        [
            Dependency::parse("zlib >= 1.3").expect("a dependency"),
            Dependency::parse("curl").expect("a dependency"),
        ]
    );
    assert_eq!(recipe.files.len(), 2);
    assert_eq!(recipe.files[0].to, "bin/ferrofetch");
    assert_eq!(recipe.files[0].mode, 0o755);
    assert_eq!(recipe.files[1].mode, 0o644);
    assert_eq!(recipe.files[0].source, Source::Build, "the default");
    assert_eq!(recipe.files[1].source, Source::Folder);
    assert_eq!(recipe.build, Build::Cargo);
    assert!(!recipe.default);
    assert!(recipe.host_tests);
    assert_eq!(recipe.smoke.len(), 2);
    assert_eq!(recipe.smoke[1].expect, "ferrofetch \"0.1\"");
}

#[test]
fn the_optional_tables_have_their_defaults() {
    let minimal = r#"
[package]
name = "a"
version = "1"
description = ""
license = "MIT"
abi = "linux"
arches = ["x86_64"]

[[package.files]]
from = "a"
to = "bin/a"
mode = "755"
"#;
    let recipe = manifest::recipe(minimal).expect("it reads");
    assert!(recipe.package.depends.is_empty());
    assert_eq!(recipe.build, Build::Cargo);
    assert!(recipe.default, "an app is in the images unless it says not");
    assert!(!recipe.host_tests);
    assert!(recipe.smoke.is_empty());
}

/// `APP` with `from` replaced by `to`, which must be refused with a message
/// containing `why`.
fn refused(from: &str, to: &str, why: &str) {
    assert!(APP.contains(from), "the sample has no {from:?}");
    let text = APP.replacen(from, to, 1);
    let error = manifest::recipe(&text).expect_err(to);
    assert!(
        error.0.contains(why),
        "{to:?}: {error} does not say {why:?}"
    );
}

#[test]
fn a_manifest_is_read_strictly() {
    refused("default = false", "defualt = false", "no key `defualt`");
    refused("[image]", "[images]", "`images` is not a table");
    refused("name = \"ferrofetch\"", "name = \"Ferro\"", "lower-case");
    refused(
        "name = \"ferrofetch\"",
        "name = \"9fetch\"",
        "from a letter",
    );
    refused(
        "version = \"0.1.0\"",
        "version = \"0.1-beta\"",
        "numbers and dots",
    );
    refused("abi = \"native\"", "abi = \"posix\"", "not native or linux");
    refused("\"armv7a\",]", "\"riscv64\"]", "`riscv64` is not one of");
    refused("\"curl\"]", "\"curl <= 2\"]", "`curl <= 2` is not");
    refused(
        "to = \"bin/ferrofetch\"",
        "to = \"/bin/ferrofetch\"",
        "leading `/`",
    );
    refused(
        "to = \"bin/ferrofetch\"",
        "to = \"bin/../etc/passwd\"",
        "`..`",
    );
    refused(
        "to = \"usr/share/ferrofetch/note.txt\"",
        "to = \"bin/ferrofetch\"",
        "installed twice",
    );
    refused("mode = \"755\"", "mode = \"rwx\"", "octal");
    refused("mode = \"755\"", "mode = \"17777\"", "octal");
    refused("kind = \"cargo\"", "kind = \"make\"", "not cargo or script");
    refused("default = false", "default = \"no\"", "true or false");
    refused(
        "host-tests = true",
        "host-tests = true\nhost-tests = false",
        "given twice",
    );
    refused("[build]", "[package]", "given twice");
    refused("mode = \"755\"", "mode = \"755", "not closed");
    refused("mode = \"755\"", "mode = 755", "a value is a string");
    refused("mode = \"755\"", "mode = \"755\" \"644\"", "text after");
    refused("mode = \"755\"", "mode = \"7\\x55\"", "an escape");
}

#[test]
fn a_license_is_an_spdx_expression() {
    for good in [
        "MIT",
        "GPL-2.0-only",
        "MIT OR Apache-2.0",
        "LGPL-2.1-or-later",
        "GPL-2.0+",
        "(MIT OR Apache-2.0) AND BSD-3-Clause",
        "GPL-2.0-only WITH Linux-syscall-note",
    ] {
        let text = APP.replacen("license = \"MIT\"", &format!("license = \"{good}\""), 1);
        let recipe = manifest::recipe(&text).unwrap_or_else(|error| panic!("{good}: {error}"));
        assert_eq!(recipe.package.license, good);
    }
    for bad in [
        "",
        "MIT OR",
        "OR MIT",
        "(MIT",
        "MIT)",
        "MIT Apache-2.0",
        "M I T",
        "9MIT",
        "MIT/X11",
    ] {
        refused(
            "license = \"MIT\"",
            &format!("license = \"{bad}\""),
            "not an SPDX expression",
        );
    }
    refused("license = \"MIT\"\n", "", "has no `license`");
}

#[test]
fn a_manifest_needs_a_package_and_a_file() {
    let error = manifest::recipe("[build]\nkind = \"cargo\"\n").expect_err("no package");
    assert!(error.0.contains("no [package]"), "{error}");
    let no_files: String = APP
        .lines()
        .filter(|line| {
            !line.starts_with("[[package.files]]")
                && !line.starts_with("from")
                && !line.starts_with("to =")
                && !line.starts_with("mode")
                && !line.starts_with("source")
        })
        .map(|line| format!("{line}\n"))
        .collect();
    let error = manifest::recipe(&no_files).expect_err("no files");
    assert!(error.0.contains("at least one file"), "{error}");
}

#[test]
fn a_file_of_the_folder_stays_in_it() {
    let folder = APP.replace(
        "from = \"share/ferrofetch.txt\"",
        "from = \"../statd/secret\"",
    );
    assert_ne!(folder, APP, "the line replaced");
    let error = manifest::recipe(&folder).expect_err("a path out of the folder");
    assert!(
        error.0.contains("not a path in the app's folder"),
        "{error}"
    );

    let elsewhere = APP.replace("source = \"folder\"", "source = \"network\"");
    assert_ne!(elsewhere, APP, "the line replaced");
    let error = manifest::recipe(&elsewhere).expect_err("an unknown source");
    assert!(error.0.contains("not `build` or `folder`"), "{error}");
}

#[test]
fn a_tree_is_taken_whole_and_has_no_mode() {
    let tree = r#"
[package]
name = "git"
version = "2.51.0"
description = "git"
license = "MIT"
abi = "linux"
arches = ["x86_64"]

[[package.files]]
from = "usr/libexec/git-core"
tree = true
to = "usr/libexec/git-core"
"#;
    let recipe = manifest::recipe(tree).expect("it reads");
    assert!(recipe.files[0].tree);
    assert_eq!(recipe.files[0].mode, manifest::TREE_MODE);

    let error = manifest::recipe(&format!("{tree}mode = \"755\"\n")).expect_err("a mode");
    assert!(error.0.contains("is a tree"), "{error}");
    let error = manifest::recipe(&tree.replace("tree = true\n", "")).expect_err("no mode");
    assert!(error.0.contains("has no `mode`"), "{error}");
    let error = manifest::recipe(&tree.replace("tree = true", "tree = \"yes\""))
        .expect_err("not a boolean");
    assert!(error.0.contains("true or false"), "{error}");
}

#[test]
fn strings_survive_being_written_and_read() {
    for text in [
        "plain",
        "a \"quote\"",
        "back\\slash",
        "tab\there",
        "line\nbreak",
        "#not a comment",
        "",
    ] {
        let mut written = String::new();
        toml::write_string(&mut written, text).expect("a String takes it");
        let document = toml::parse(&format!("[t]\nk = {written}\n")).expect("it reads back");
        let value = document.table("t").and_then(|table| table.get("k"));
        assert_eq!(value, Some(&Value::String(text.to_owned())), "{written}");
    }
}

#[test]
fn versions_compare_number_by_number() {
    let v = |text| Version::parse(text).expect(text);
    assert!(v("1.10") > v("1.9"));
    assert!(v("1.2") == v("1.2"));
    assert_eq!(v("1.2").cmp(&v("1.2.0")), core::cmp::Ordering::Equal);
    assert!(v("2") > v("1.99.99"));
    for bad in ["", "1.", ".1", "1..2", "a", "1.2.3.4.5", "99999999999"] {
        assert_eq!(Version::parse(bad), None, "{bad:?}");
    }
    let needs = Dependency::parse("zlib >= 1.3").expect("a dependency");
    assert!(needs.accepts(&v("1.3")));
    assert!(needs.accepts(&v("1.3.1")));
    assert!(!needs.accepts(&v("1.2.13")));
    assert!(
        Dependency::parse("zlib")
            .expect("a name")
            .accepts(&v("0.1"))
    );
}

/// A record of `name` at `version` owning `paths`, needing `depends`.
fn package(name: &str, version: &str, depends: &[&str], paths: &[&str]) -> Record {
    let text = format!(
        "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"\"\nlicense = \"MIT\"\nabi = \"linux\"\n\
         arches = [\"x86_64\"]\ndepends = [{}]\n{}",
        depends
            .iter()
            .map(|d| format!("\"{d}\""))
            .collect::<Vec<_>>()
            .join(", "),
        paths
            .iter()
            .map(|path| {
                let file = Installed::of(path, 0o644, path.as_bytes());
                let mut hex = String::new();
                for byte in file.digest {
                    hex.push_str(&format!("{byte:02x}"));
                }
                format!(
                    "\n[[files]]\npath = \"{path}\"\nmode = \"644\"\nsize = \"{}\"\nblake2b = \"{hex}\"\n",
                    path.len()
                )
            })
            .collect::<String>(),
    );
    record::parse(&text).expect("a record")
}

#[test]
fn a_record_reads_back_what_was_written() {
    let recipe = manifest::recipe(APP).expect("it reads");
    let mut built = recipe.package;
    built.arches = vec!["aarch64".to_owned()];
    let record = Record {
        package: built,
        files: vec![
            Installed::of("bin/ferrofetch", 0o755, b"\x7fELF..."),
            Installed::of("usr/share/ferrofetch/note.txt", 0o644, b""),
            Installed::link("usr/bin/ferrofetch", "../../bin/ferrofetch"),
        ],
    };
    let text = record::render(&record);
    assert_eq!(record::parse(&text), Ok(record.clone()));
    assert!(
        text.contains("path = \"usr/bin/ferrofetch\"\nlink = \"../../bin/ferrofetch\"\n"),
        "{text}"
    );
    assert!(
        text.contains("depends = [\"zlib >= 1.3\", \"curl\"]"),
        "{text}"
    );
    assert!(text.contains("mode = \"755\""), "{text}");
    // The empty file's digest is BLAKE2b-256 of nothing.
    assert!(
        text.contains("0e5751c026e543b2e8ab2eb06099daa1d1e5df47778f7787faab45cdf12fe3a8"),
        "{text}"
    );
    assert_eq!(
        record::path("ferrofetch"),
        "lib/ferrix/packages/ferrofetch.toml"
    );
    let paths: Vec<String> = record.paths().collect();
    assert_eq!(
        paths,
        [
            "bin/ferrofetch",
            "usr/share/ferrofetch/note.txt",
            "usr/bin/ferrofetch",
            "lib/ferrix/packages/ferrofetch.toml",
        ]
    );
}

#[test]
fn a_record_is_for_one_architecture_and_safe_paths() {
    let mut text = record::render(&package("a", "1", &[], &["bin/a"]));
    text = text.replace(
        "arches = [\"x86_64\"]",
        "arches = [\"x86_64\", \"aarch64\"]",
    );
    assert!(record::parse(&text).is_err());
    let unsafe_path = record::render(&package("a", "1", &[], &["bin/a"])).replace("bin/a", "../a");
    assert!(record::parse(&unsafe_path).is_err());
    let short = record::render(&package("a", "1", &[], &["bin/a"])).replacen(
        "blake2b = \"",
        "blake2b = \"0",
        1,
    );
    assert!(record::parse(&short).is_err());
}

#[test]
fn the_plan_puts_dependencies_first() {
    let set = [
        package("git", "2.47", &["zlib >= 1.3", "curl"], &["bin/git"]),
        package("curl", "8.11", &["zlib"], &["bin/curl"]),
        package("zlib", "1.3.1", &[], &["lib/libz.so"]),
        package("ferrofetch", "0.1.0", &[], &["bin/ferrofetch"]),
    ];
    assert_eq!(plan(&set), Ok(vec![2, 1, 0, 3]));
    assert_eq!(plan(&[]), Ok(vec![]));
}

/// The plan's refusal of `set`, which must mention `why`.
fn plan_refuses(set: &[Record], why: &str) {
    let error = plan(set).expect_err(why);
    assert!(error.0.contains(why), "{error} does not say {why:?}");
}

#[test]
fn the_plan_refuses_a_set_that_does_not_install() {
    plan_refuses(
        &[package("git", "2", &["zlib"], &["bin/git"])],
        "`git` needs `zlib`, which is not in the set",
    );
    plan_refuses(
        &[
            package("git", "2", &["zlib >= 1.3"], &["bin/git"]),
            package("zlib", "1.2.13", &[], &["lib/libz.so"]),
        ],
        "the set has 1.2.13",
    );
    plan_refuses(
        &[
            package("a", "1", &[], &["bin/a"]),
            package("a", "2", &[], &["bin/a2"]),
        ],
        "`a` is in the set twice",
    );
    plan_refuses(
        &[
            package("a", "1", &[], &["bin/tool"]),
            package("b", "1", &[], &["bin/tool"]),
        ],
        "`bin/tool` is installed by both `a` and `b`",
    );
    // A file where another package's record goes.
    plan_refuses(
        &[
            package("a", "1", &[], &["lib/ferrix/packages/b.toml"]),
            package("b", "1", &[], &["bin/b"]),
        ],
        "installed by both",
    );
    plan_refuses(
        &[
            package("a", "1", &["b"], &["bin/a"]),
            package("b", "1", &["c"], &["bin/b"]),
            package("c", "1", &["a"], &["bin/c"]),
        ],
        "depends on itself",
    );
    plan_refuses(
        &[package("a", "1", &["a"], &["bin/a"])],
        "depends on itself",
    );
}
