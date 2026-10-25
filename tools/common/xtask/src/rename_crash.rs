//! `test-rename-crash`: a file written under a new name and renamed over an
//! existing one with `mv -f`, never synced, then the power cut. The next boot
//! must find the old file or the new one whole, never the name on an empty
//! file (`src/lib/fs/btrfs/vfs/src/rw.rs`, "A file renamed over another takes
//! its bytes along").
//!
//! Seen on the N150 box on 2026-10-25: Wine's `ntdll.so`, replaced so, was 0
//! bytes after a cold power cycle seven minutes later. Linux promises no
//! more than the old file or the new one without an `fsync`, and its btrfs
//! and ext4 both write a file renamed over another back early so that it is
//! not lost (`btrfs_rename`'s `filemap_flush`, ext4's `auto_da_alloc`).
//!
//! Three boots of one image, a shell as init, `/` on a btrfs root made fresh
//! for the first and kept for the others:
//!
//! 1. **The committer alone.** `/rc-timer` is written and synced, a new copy
//!    is written beside it and moved over it, and nothing is synced: the
//!    guest is left for [`COMMITS`] -- past three of the root's 30 s commit
//!    intervals -- and the power is cut.
//! 2. That boot's file must be the new one whole: the periodic commits
//!    reached the disk. Then **an `fsync` of another file**: `/rc-fsync` is
//!    replaced as above, `/rc-other` is written and `sync -d`ed -- an
//!    `fdatasync`, which commits the shape of the tree, and with it the
//!    rename -- and the power is cut at once, before the next periodic
//!    commit. (`sync FILE` without `-d` or `-f` is uutils' whole `sync`,
//!    which writes every file back and so proves nothing here.)
//! 3. `/rc-fsync` must be the old file or the new one whole, and both files
//!    of the earlier boots as they were left; then host `btrfs check` reads
//!    the volume.
//!
//! The second boot's cut is what failed before the fix: the `fsync`'s commit
//! carried the rename and not the moved file's bytes, and the file was 0
//! bytes long. Its negative control is that fix taken out.
//!
//! x86-64 only, for the reason `test-restart` is: `seq`, `mv` and `sync` are
//! uutils', built for x86-64 alone (`docs/UUTILS.md` D3).

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, btrfs_check, btrfs_disk, cargo, fat, initramfs, native, qemu};
use crate::{uutils, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-rename-crash",
    help_at: 185,
    help: &[
        "test-rename-crash  Replace a file on a btrfs root with mv -f, cut the power after the periodic commits and",
        "              after an fsync of another file, and require the old file or the new one whole at each next boot",
    ],
    run: |_, args| test(args),
}];

/// How long to wait for the answer to one line.
const PATIENCE: Duration = Duration::from_secs(60);

/// How long the first boot leaves its rename to the periodic commits: past
/// three of the root's 30 s intervals.
const COMMITS: Duration = Duration::from_secs(100);

/// What the kernel prints once `/` is the btrfs root and its committer runs.
const ROOT_LINE: &str = "/ is btrfs on";

/// What the kernel prints when a periodic commit of `/` failed.
const COMMIT_FAILED: &str = "the periodic commit of / failed";

/// What the guest's answers begin with. Typed with a quote in the middle,
/// so the echo of the typed line never matches it.
const TAG: &str = "rc-gate-";

/// The old file: `seq 1 20000`, 108,894 bytes, whose first line is `1`.
const OLD: (&str, u64, &str) = ("1 20000", 108_894, "1");

/// The new file: `seq 100001 120000`, 140,000 bytes, whose first line is
/// `100001`.
const NEW: (&str, u64, &str) = ("100001 120000", 140_000, "100001");

/// `test-rename-crash`.
fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-rename-crash runs on x86-64 only: `seq`, `mv` and `sync` come from uutils, \
             which is built for x86-64 alone (docs/UUTILS.md D3)",
        ));
    }
    let checker = btrfs_check::Checker::required()?;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose init is an interactive shell");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, "")?;
    let natives = native::build(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    if utilities.is_empty() {
        return Err(Error::new(
            "the image carries no utilities: `cargo xtask uutils` builds the ones this gate types",
        ));
    }
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let archive =
        initramfs::build_with_utilities(Some(&shell), &natives, Some(&bytes), &utilities, &[])?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    let mut booted = args.clone();
    booted.btrfs_root = true;

    // Each boot after the first finds the root as the cut left it.
    btrfs_disk::keep_test_root(false);
    let outcome = boots(arch, &image, &kernel, &booted);
    btrfs_disk::keep_test_root(false);
    outcome?;
    checker.run(&btrfs_disk::test_root_path(arch), arch)?;
    println!(
        "  {arch}: a file replaced by mv -f was whole after the periodic commits and after \
         an fsync of another file, each followed by a power cut, and btrfs check passed"
    );
    Ok(())
}

/// The three boots, each judged as it ends.
fn boots(arch: Arch, image: &std::path::Path, kernel: &std::path::Path, args: &Args) -> Result<()> {
    println!(
        "  {arch}: boot 1 of 3: /rc-timer replaced, then {} s for the periodic commits, then \
         the power cut",
        COMMITS.as_secs()
    );
    boot(arch, image, kernel, args, |watching| {
        ready(watching)?;
        replace(watching, "rc-timer")?;
        let quiet = watching
            .read_more(Instant::now() + COMMITS, |lines| {
                lines
                    .iter()
                    .any(|line| line.contains(qemu::PANIC_MARKER) || line.contains(COMMIT_FAILED))
            })
            .map_err(|error| error.to_string())?;
        if quiet {
            return Err("the kernel panicked or a periodic commit failed".to_owned());
        }
        watching.cut_power();
        Ok(())
    })?;
    btrfs_disk::keep_test_root(true);

    println!(
        "  {arch}: boot 2 of 3: /rc-timer read back; /rc-fsync replaced, another file \
         fsynced, the power cut at once"
    );
    boot(arch, image, kernel, args, |watching| {
        ready(watching)?;
        expect(watching, "rc-timer", &[NEW])?;
        replace(watching, "rc-fsync")?;
        let answer = say(
            watching,
            "echo line > /rc-other && sync -d /rc-other && echo rc-gate-'fsynced=yes'\n",
            "fsynced",
        )?;
        if answer != "yes" {
            return Err(format!("the fsync of /rc-other answered {answer:?}"));
        }
        watching.cut_power();
        Ok(())
    })?;

    println!("  {arch}: boot 3 of 3: both files read back");
    boot(arch, image, kernel, args, |watching| {
        ready(watching)?;
        expect(watching, "rc-fsync", &[OLD, NEW])?;
        expect(watching, "rc-timer", &[NEW])?;
        // A clean end: the power-off commits nothing the check needs, and
        // the volume is read on the host next.
        let _ = say(watching, "sync && echo rc-gate-'synced=yes'\n", "synced")?;
        watching.stop_when_done();
        Ok(())
    })
}

/// One boot: `hook` types at the shell, and its failure is the gate's.
fn boot(
    arch: Arch,
    image: &std::path::Path,
    kernel: &std::path::Path,
    args: &Args,
    mut hook: impl FnMut(&mut qemu::Watching<'_>) -> std::result::Result<(), String>,
) -> Result<()> {
    let mut failure = None;
    let lines = qemu::watch_then(
        arch,
        image,
        kernel,
        args,
        qemu::SUCCESS_MARKER,
        |watching| {
            if let Err(why) = hook(watching) {
                watching.stop_when_done();
                failure = Some(why);
            }
            Ok(())
        },
    )?;
    if let Some(panic) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Err(Error::new(format!(
            "{arch}: the kernel panicked: {}",
            panic.trim()
        )));
    }
    match failure {
        Some(why) => Err(Error::new(format!(
            "{arch}: test-rename-crash: {why}\n  The whole transcript is above and in the \
             serial log."
        ))),
        None => Ok(()),
    }
}

/// Wait until `/` is the btrfs root and the shell reads the console.
fn ready(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    let rooted = |lines: &[String]| lines.iter().any(|line| line.contains(ROOT_LINE));
    let seen = rooted(watching.lines())
        || watching
            .read_more(deadline, rooted)
            .map_err(|error| error.to_string())?;
    if !seen {
        return Err(format!("`/` never became the btrfs root: no `{ROOT_LINE}`"));
    }
    if watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("the shell never answered at the console".to_owned())
    }
}

/// Write `/<name>` with the old file and sync it, then write the new file
/// beside it and move it over the old one, syncing nothing.
fn replace(watching: &mut qemu::Watching<'_>, name: &str) -> std::result::Result<(), String> {
    let (old, _, _) = OLD;
    let (new, _, _) = NEW;
    let written = say(
        watching,
        &format!("seq {old} > /{name} && sync && echo rc-gate-'old=yes'\n"),
        "old",
    )?;
    let moved = say(
        watching,
        &format!(
            "seq {new} > /{name}.tmp && mv -f /{name}.tmp /{name} && echo rc-gate-'moved=yes'\n"
        ),
        "moved",
    )?;
    if written == "yes" && moved == "yes" {
        Ok(())
    } else {
        Err(format!(
            "replacing /{name} answered {written:?} and {moved:?}"
        ))
    }
}

/// Require `/<name>` to be one of `whole`, by its length and first line.
fn expect(
    watching: &mut qemu::Watching<'_>,
    name: &str,
    whole: &[(&str, u64, &str)],
) -> std::result::Result<(), String> {
    let size = say(
        watching,
        &format!("echo rc-gate-'size='$(wc -c < /{name})\n"),
        "size",
    )?;
    let first = say(
        watching,
        &format!("echo rc-gate-'first='$(head -n 1 /{name})\n"),
        "first",
    )?;
    let found = whole
        .iter()
        .any(|&(_, bytes, line)| size == bytes.to_string() && first == line);
    println!("  /{name} after the cut: {size} bytes, first line {first:?}");
    if found {
        Ok(())
    } else {
        let wanted: Vec<String> = whole
            .iter()
            .map(|&(_, bytes, line)| format!("{bytes} bytes from {line}"))
            .collect();
        Err(format!(
            "/{name} was {size} bytes, first line {first:?}, after the power cut, not {}: \
             the rename reached the disk without the file's bytes",
            wanted.join(" or ")
        ))
    }
}

/// Type `line`, and give back the word after `rc-gate-<key>=` in the first
/// answer that has it ("" for an empty answer).
fn say(
    watching: &mut qemu::Watching<'_>,
    line: &str,
    key: &str,
) -> std::result::Result<String, String> {
    let tag = format!("{TAG}{key}=");
    let before = watching.after().len();
    watching
        .type_in(line.as_bytes())
        .map_err(|error| error.to_string())?;
    let answered = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(&tag))
        })
        .map_err(|error| error.to_string())?;
    if !answered {
        return Err(format!(
            "no answer to {:?} within {}s",
            line.trim(),
            PATIENCE.as_secs()
        ));
    }
    Ok(watching
        .after()
        .get(before..)
        .unwrap_or_default()
        .iter()
        .find_map(|line| line.split_once(&tag))
        .map(|(_, rest)| {
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_default())
}
