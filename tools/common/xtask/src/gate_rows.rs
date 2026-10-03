//! `cargo xtask gate-rows --since <ref>`: which of `docs/BACKLOG.md`'s gate
//! rows a change has to run, from the paths it changes (`docs/TEST-TIME.md`,
//! Phase 3, B1).
//!
//! The table *What a landing runs* says, by what a change touches, which
//! gates land it. Applying it by hand is where time went: a docs edit paid
//! the whole `check`, and a branch rebased onto a moved `main` ran its whole
//! row again whether or not the commits that moved it came near its files.
//! This reads the table's columns as code and prints the commands. It never
//! checks less than the table: a path no row names gets the image row, the
//! widest, and says so, and what the table leaves to judgement ("a stage 7
//! change also runs ...") is printed as a question beside the rows.
//!
//! With `--moved <old main>` it answers the rebase question instead: whether
//! the commits between the old base and `main` touched any file the branch
//! does. None, and the gate stands (`docs/CONVENTIONS.md`, splitting rule 3).

use std::collections::BTreeSet;
use std::process::Command;

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// What a change's paths ask of its gate.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Rows {
    /// The commands, in the order to run them.
    pub(crate) commands: Vec<String>,
    /// Why each part of the gate is there, one line a reason.
    pub(crate) reasons: Vec<String>,
    /// What the table leaves to whoever knows the change.
    pub(crate) questions: Vec<String>,
}

impl Rows {
    fn add(&mut self, commands: &[&str]) {
        for command in commands {
            if !self.commands.iter().any(|known| known == command) {
                self.commands.push((*command).to_owned());
            }
        }
    }

    fn ask(&mut self, question: &str) {
        if !self.questions.iter().any(|known| known == question) {
            self.questions.push(question.to_owned());
        }
    }
}

/// The table's image row: anything the image contains, and the default.
/// Both `x86_64` accelerators are named, so the row means the same thing on
/// every host (`docs/BACKLOG.md`, *Name the accelerator*).
const IMAGE_ROW: &[&str] = &[
    "check",
    "build --arch all --release",
    "test-boot --arch x86_64 --accel tcg",
    "test-boot --arch x86_64 --accel kvm",
    "test-boot --arch aarch64",
    "test-boot --arch armv7a",
    "test-boot --arch armv7a --smp 2",
];

/// Whether `path` is documentation only: under `docs/`, or a Markdown file
/// at the top of the tree.
fn is_docs(path: &str) -> bool {
    path.starts_with("docs/") || (!path.contains('/') && path.ends_with(".md"))
}

/// The rows for `changed`, given the directories (relative, with `/`) of the
/// crates the kernel and the loader build.
pub(crate) fn rows(changed: &[String], image_crates: &BTreeSet<String>) -> Rows {
    let mut rows = Rows::default();
    if changed.is_empty() {
        rows.reasons
            .push("nothing committed changes anything (gate-rows reads commits)".to_owned());
        return rows;
    }
    if changed.iter().all(|path| is_docs(path)) {
        rows.add(&["check-docs"]);
        rows.reasons
            .push("only docs/ and top-level Markdown: the steps that read them".to_owned());
        return rows;
    }
    for path in changed.iter().filter(|path| !is_docs(path)) {
        rows_for(path, image_crates, &mut rows);
    }
    rows
}

/// What one path adds to `rows`.
fn rows_for(path: &str, image_crates: &BTreeSet<String>, rows: &mut Rows) {
    let under = |prefix: &str| path.starts_with(prefix);
    let image_crate = image_crates
        .iter()
        .any(|dir| path.starts_with(&format!("{dir}/")));
    let reason = |rows: &mut Rows, why: &str| rows.reasons.push(format!("{path}: {why}"));
    if under("src/user/system/linux/ferrousli/") {
        rows.add(&[
            "check --ferrousli",
            "busybox",
            "test-shell --arch x86_64 --init ferrousli",
            "test-vfs --arch x86_64 --init ferrousli",
            "uutils",
        ]);
        reason(rows, "ferrousli's row");
        if under("src/user/system/linux/ferrousli/tools/ports/") {
            rows.add(&[
                "ports",
                "test-net --arch x86_64 --init ferrousli",
                "ports --arch aarch64",
                "ports --arch armv7a",
                "test-net --arch all",
            ]);
            reason(rows, "ferrousli's ports");
        }
    } else if under("src/user/system/linux/zinc/") {
        rows.add(&["check --fast --zinc"]);
        reason(rows, "zinc's row");
        rows.ask(
            "zinc: does the change alter what zinc does at boot? then test-boot --arch x86_64",
        );
        rows.ask(
            "zinc: does it change how zinc starts, waits for or signals a process? then \
             test-shell --arch all and test-jobs",
        );
    } else if under("src/user/system/linux/init/") || under("src/lib/init/svc/") {
        rows.add(&["check", "test-init --arch all"]);
        reason(rows, "init's row");
    } else if under("src/user/system/linux/auth/") || path == "tools/common/xtask/src/auth.rs" {
        rows.add(IMAGE_ROW);
        rows.add(&["test-auth --arch all"]);
        reason(rows, "authd's row: the image row, then test-auth");
        rows.ask(
            "authd: does it change what authd refuses? then test-auth --arch x86_64 \
             --sabotage NAME for each name in src/user/system/linux/auth/authd/src/sabotage.rs",
        );
    } else if under("src/kernel/")
        || under("src/boot/common/uefi/")
        || under("tools/common/xtask/")
        || image_crate
        || path == "Cargo.toml"
        || path == "Cargo.lock"
        || under(".cargo/")
    {
        rows.add(IMAGE_ROW);
        reason(rows, "in the image: the image row");
        rows.ask(
            "stage 7 or 8? then test-shell --arch x86_64 (zinc, --init ferrousli, the musl and \
             the glibc busybox) and test-vfs --arch x86_64 (ferrousli and musl busybox)",
        );
        rows.ask("stage 7? then test-threads --arch all");
        rows.ask("the loader, exec or ferrousli? then test-shell with Debian's dynamic busybox");
    } else if under("src/lib/") {
        rows.add(&["check", "test-boot --arch armv7a --smp 2"]);
        reason(rows, "a library no image crate uses: check and one boot");
    } else {
        rows.add(IMAGE_ROW);
        reason(
            rows,
            "no row of the table names this path, so the widest one; narrow it by hand if \
             you know better, and say why",
        );
    }
}

/// The directories of the crates the kernel and the loader build, relative
/// to the workspace, from `cargo tree` for each architecture's two targets.
fn image_crates() -> Result<BTreeSet<String>> {
    let root = paths::workspace_root();
    let mut found = BTreeSet::new();
    for arch in Arch::ALL {
        for (package, target) in [
            ("ferrix-kernel", arch.kernel_target()),
            ("ferrix-boot", arch.loader_target()),
        ] {
            let output = Command::new(crate::cargo::cargo())
                .current_dir(&root)
                .args(["tree", "-q", "-p", package, "--target", target])
                .args(["-e", "normal,build", "--prefix", "none"])
                .output()
                .map_err(|error| Error::new(format!("could not run cargo tree: {error}")))?;
            crate::cargo::finished(output.status, "cargo tree")?;
            for line in String::from_utf8_lossy(&output.stdout).lines() {
                let Some(path) = line
                    .rsplit_once('(')
                    .and_then(|(_, rest)| rest.strip_suffix(')'))
                else {
                    continue;
                };
                let path = std::path::Path::new(path.trim_end_matches(" (*)"));
                if let Ok(relative) = path.strip_prefix(&root) {
                    let _ = found.insert(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    Ok(found)
}

/// What BACKLOG's landing rule calls cross-cutting: "nothing cross-cutting
/// (locks, the scheduler, the trap or system-call entry, memory
/// management)". A `main` that moved in any of these asks for the gate
/// again, whichever files the branch touches. Each entry is a module of
/// `src/kernel/src` (its `.rs` and its directory) or, ending in `/`, a
/// directory.
const CROSS_CUTTING: &[&str] = &[
    "src/kernel/src/sched",
    "src/kernel/src/trap",
    "src/kernel/src/mm",
    "src/kernel/src/user/",
    "src/kernel/src/smp",
    "src/kernel/src/sync",
    "src/lib/kernel/sync/",
];

/// The parts of an architecture's code that are the trap and system-call
/// entry and the switch: `src/kernel/src/arch/<arch>/<one of these>`.
const CROSS_CUTTING_ARCH: &[&str] = &["trap", "syscall", "entry", "switch"];

/// Whether a change to `path` is cross-cutting in BACKLOG's sense.
fn cross_cutting(path: &str) -> bool {
    let module = |name: &str| path == format!("{name}.rs") || path.starts_with(&format!("{name}/"));
    let listed = CROSS_CUTTING.iter().any(|entry| {
        if entry.ends_with('/') {
            path.starts_with(entry)
        } else {
            module(entry)
        }
    });
    let arch = path
        .strip_prefix("src/kernel/src/arch/")
        .and_then(|rest| rest.split_once('/'))
        .is_some_and(|(_, part)| CROSS_CUTTING_ARCH.iter().any(|name| part.starts_with(name)));
    listed || arch
}

/// The certification item's manifest, whose `core` and `item` rings name
/// the kernel files, and whose `crates` the library crates, a change to
/// which the consultant reviews.
const MANIFEST: &str = "tools/common/data/certification-item.json";

/// The paths of `changed` that need the certification consultant's review
/// before `land.sh take` (`docs/CONVENTIONS.md`, *Changes to the certified
/// item go through review*): the manifest, `docs/certification/`,
/// `docs/sysml/`, `tools/common/data/`, a kernel file in the `core` or
/// `item` ring (`members`, relative to `src/kernel/src`, exact or `dir/**`),
/// any file of a `core` or `item` crate (`crates`, its directory: sources,
/// tests and `Cargo.toml` alike), and every path in `added_unsafe`.
fn needs_review(
    changed: &[String],
    members: &[String],
    crates: &[String],
    added_unsafe: &[String],
) -> Vec<String> {
    let in_ring = |path: &str| {
        path.strip_prefix("src/kernel/src/")
            .is_some_and(|relative| {
                members
                    .iter()
                    .any(|member| match member.strip_suffix("/**") {
                        Some(dir) => relative.starts_with(&format!("{dir}/")),
                        None => relative == member,
                    })
            })
    };
    changed
        .iter()
        .filter(|path| {
            path.as_str() == MANIFEST
                || path.starts_with("docs/certification/")
                || path.starts_with("docs/sysml/")
                || path.starts_with("tools/common/data/")
                || in_ring(path)
                || crates
                    .iter()
                    .any(|dir| path.starts_with(&format!("{}/", dir.trim_end_matches('/'))))
                || added_unsafe.contains(path)
        })
        .cloned()
        .collect()
}

/// The `core` and `item` rings' members, and the directories of the `core`
/// and `item` crates, read from the manifest itself.
fn ring_members() -> Result<(Vec<String>, Vec<String>)> {
    let interpreter = crate::check::python_interpreter()
        .ok_or_else(|| Error::new("no working Python interpreter on PATH"))?;
    let output = Command::new(interpreter)
        .current_dir(paths::workspace_root())
        .args([
            "-c",
            "import json, sys\n\
             manifest = json.load(open(sys.argv[1], encoding='utf-8'))\n\
             rings = manifest['rings']\n\
             crates = manifest.get('crates', {}).get('members', {}).values()\n\
             print('\\n'.join(['file ' + m for r in ('core', 'item') for m in rings[r]['members']]\n\
                 + ['crate ' + c['path'] for c in crates if c['ring'] in ('core', 'item')]))",
            MANIFEST,
        ])
        .output()
        .map_err(|error| Error::new(format!("could not read {MANIFEST}: {error}")))?;
    crate::cargo::finished(output.status, "reading the certification item's rings")?;
    let mut members = Vec::new();
    let mut crates = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(member) = line.strip_prefix("file ") {
            members.push(member.to_owned());
        } else if let Some(dir) = line.strip_prefix("crate ") {
            crates.push(dir.to_owned());
        }
    }
    Ok((members, crates))
}

/// The kernel files to which the commits since `since` add a line holding
/// `unsafe`.
fn added_unsafe(since: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(paths::workspace_root())
        .args([
            "diff",
            "--unified=0",
            &format!("{since}...HEAD"),
            "--",
            "src/kernel",
        ])
        .output()
        .map_err(|error| Error::new(format!("could not run git diff: {error}")))?;
    crate::cargo::finished(output.status, "git diff")?;
    let mut found = Vec::new();
    let mut file = None;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(path) = line.strip_prefix("+++ b/") {
            file = Some(path.to_owned());
        } else if line.starts_with('+')
            && !line.starts_with("+++")
            && line.contains("unsafe")
            && let Some(path) = file.take()
        {
            found.push(path);
        }
    }
    Ok(found)
}

/// The files changed between `from` and `to`.
fn changed(from: &str, to: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(paths::workspace_root())
        .args(["diff", "--name-only", &format!("{from}...{to}")])
        .output()
        .map_err(|error| Error::new(format!("could not run git diff: {error}")))?;
    crate::cargo::finished(output.status, "git diff --name-only")?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `gate-rows`: print the rows for the branch's changes since `--since`, or
/// with `--moved`, whether a rebase onto `main` asks for a gate again.
///
/// # Errors
///
/// No `--since`, or git or cargo failing.
pub(crate) fn run(args: &Args) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .ok_or_else(|| Error::new("gate-rows needs --since <ref>, the branch's base (main)"))?;
    let branch = changed(since, "HEAD")?;
    // First, since it holds whether the gate stands or not.
    let (members, crates) = ring_members()?;
    let review = needs_review(&branch, &members, &crates, &added_unsafe(since)?);
    if !review.is_empty() {
        println!(
            "needs the certification consultant's review before land.sh take \
             (docs/CONVENTIONS.md):"
        );
        for path in &review {
            println!("  {path}");
        }
    }
    if let Some(old) = args.moved.as_deref() {
        let moved = changed(old, since)?;
        let both: Vec<&String> = branch.iter().filter(|path| moved.contains(path)).collect();
        let crossing: Vec<&String> = moved.iter().filter(|path| cross_cutting(path)).collect();
        for path in &crossing {
            println!(
                "re-gate: {since} moved in {path}, cross-cutting (docs/BACKLOG.md, Landing on main)"
            );
        }
        if both.is_empty() && crossing.is_empty() {
            println!(
                "none of the {} files changed between {old} and {since} is one of the branch's {}: \
                 the gate stands (docs/CONVENTIONS.md, rule 3); say so in the report",
                moved.len(),
                branch.len()
            );
            return Ok(());
        }
        if !both.is_empty() {
            println!("{since} moved in files the branch changes; gate again:");
            for path in both {
                println!("  {path}");
            }
        }
    }
    let rows = rows(&branch, &image_crates()?);
    println!(
        "{} paths changed since {since}; docs/BACKLOG.md, What a landing runs:",
        branch.len()
    );
    for reason in &rows.reasons {
        println!("  {reason}");
    }
    println!("\nrun:");
    for command in &rows.commands {
        println!("  cargo xtask {command}");
    }
    if !rows.questions.is_empty() {
        println!("\nand if the change is one of these, add what it says:");
        for question in &rows.questions {
            println!("  {question}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows_of(paths: &[&str]) -> Rows {
        let crates = BTreeSet::from(["src/lib/proto/blkring".to_owned()]);
        rows(
            &paths
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>(),
            &crates,
        )
    }

    #[test]
    fn a_docs_change_runs_only_the_docs_steps() {
        let rows = rows_of(&[
            "docs/BACKLOG.md",
            "README.md",
            "docs/sysml/10-roadmap.sysml",
        ]);
        assert_eq!(rows.commands, ["check-docs"]);
    }

    #[test]
    fn docs_beside_code_change_nothing_about_the_code_row() {
        let rows = rows_of(&["docs/BACKLOG.md", "src/kernel/src/main.rs"]);
        assert_eq!(rows.commands, IMAGE_ROW);
    }

    #[test]
    fn a_kernel_crate_in_src_lib_is_the_image_row_and_another_lib_one_boot() {
        assert_eq!(
            rows_of(&["src/lib/proto/blkring/src/lib.rs"]).commands,
            IMAGE_ROW
        );
        assert_eq!(
            rows_of(&["src/lib/proto/other/src/lib.rs"]).commands,
            ["check", "test-boot --arch armv7a --smp 2"]
        );
    }

    #[test]
    fn a_path_the_table_does_not_name_gets_the_widest_row() {
        let rows = rows_of(&["src/user/apps/no-such-app/build.sh"]);
        assert_eq!(rows.commands, IMAGE_ROW);
        assert!(rows.reasons[0].contains("no row of the table"));
    }

    #[test]
    fn rows_are_joined_without_repeats() {
        let rows = rows_of(&[
            "src/user/system/linux/auth/authd/src/main.rs",
            "src/kernel/src/main.rs",
        ]);
        assert_eq!(rows.commands.iter().filter(|c| *c == "check").count(), 1);
        assert_eq!(
            rows.commands.last().map(String::as_str),
            Some("test-auth --arch all")
        );
    }

    #[test]
    fn ferrousli_ports_add_the_network_rows() {
        let rows = rows_of(&["src/user/system/linux/ferrousli/tools/ports/curl/build.sh"]);
        assert!(rows.commands.iter().any(|c| c == "test-net --arch all"));
        assert_eq!(rows.commands[0], "check --ferrousli");
    }

    #[test]
    fn a_main_moved_in_locks_scheduling_traps_or_memory_is_cross_cutting() {
        for path in [
            "src/kernel/src/sched/queue.rs",
            "src/kernel/src/sched.rs",
            "src/kernel/src/mm.rs",
            "src/kernel/src/mm/space.rs",
            "src/kernel/src/trap.rs",
            "src/kernel/src/sync.rs",
            "src/kernel/src/smp/ipi.rs",
            "src/kernel/src/user/copy.rs",
            "src/kernel/src/arch/x86_64/syscall.rs",
            "src/kernel/src/arch/aarch64/trap/vectors.rs",
            "src/kernel/src/arch/armv7a/switch.rs",
            "src/lib/kernel/sync/src/lib.rs",
        ] {
            assert!(cross_cutting(path), "{path}");
        }
        for path in [
            "src/kernel/src/mmio.rs",
            "src/kernel/src/fs/tmpfs.rs",
            "src/kernel/src/arch/x86_64/cpu.rs",
            "docs/BACKLOG.md",
        ] {
            assert!(!cross_cutting(path), "{path}");
        }
    }

    #[test]
    fn review_is_asked_for_the_rings_the_evidence_and_new_unsafe() {
        let members = vec!["audit.rs".to_owned(), "arch/**".to_owned()];
        let crates = vec!["src/lib/fs/btrfs".to_owned()];
        let changed: Vec<String> = [
            "src/lib/fs/btrfs/Cargo.toml",
            "src/lib/fs/btrfs-vfs/src/lib.rs",
            "src/kernel/src/audit.rs",
            "src/kernel/src/arch/x86_64/cpu.rs",
            "src/kernel/src/fs/tmpfs.rs",
            "src/kernel/src/net/tcp.rs",
            "docs/certification/ITEM.md",
            "docs/sysml/10-roadmap.sysml",
            "tools/common/data/certification-item.json",
            "docs/BACKLOG.md",
            "tools/common/xtask/src/main.rs",
        ]
        .map(str::to_owned)
        .to_vec();
        let unsafe_added = vec!["src/kernel/src/net/tcp.rs".to_owned()];
        assert_eq!(
            needs_review(&changed, &members, &crates, &unsafe_added),
            [
                "src/lib/fs/btrfs/Cargo.toml",
                "src/kernel/src/audit.rs",
                "src/kernel/src/arch/x86_64/cpu.rs",
                "src/kernel/src/net/tcp.rs",
                "docs/certification/ITEM.md",
                "docs/sysml/10-roadmap.sysml",
                "tools/common/data/certification-item.json",
            ]
        );
    }

    #[test]
    fn both_x86_accelerators_are_named_in_the_image_row() {
        assert!(IMAGE_ROW.contains(&"test-boot --arch x86_64 --accel tcg"));
        assert!(IMAGE_ROW.contains(&"test-boot --arch x86_64 --accel kvm"));
    }
}
