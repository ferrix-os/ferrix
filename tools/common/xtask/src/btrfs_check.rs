//! Stage 12's exit: boot, write a tree on a blank btrfs volume, and let host
//! `btrfs check` judge what was written.
//!
//! The guest's half is `src/kernel/src/fs/btrfs_write_check.rs`, which builds a
//! tree on the third disk, unmounts, mounts again and reads it all back; it
//! proves the bytes survive a round trip through Ferrix. It cannot prove the
//! trees are the ones btrfs would have written — a writer can be
//! self-consistent and still wrong about the format. That is what this
//! command adds: after the boot, `btrfs check --check-data-csum` from
//! btrfs-progs reads the same image, checks every tree against every other
//! and every data checksum against the data, and any complaint fails the
//! command.
//!
//! btrfs-progs must be on the host. Without it the command fails rather than
//! passing quietly: a gate that skips is not a gate. On Windows it is looked
//! for in WSL as well, since that is where the rest of the Linux tooling
//! lives.

use std::path::Path;
use std::process::Command;

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-btrfs",
    help_at: 170,
    help: &[
        "test-btrfs    Boot, write a tree on the blank btrfs disk, and require host btrfs check to find nothing",
    ],
    run: |_, args| test_btrfs(args, |arch| crate::build_image(arch, args)),
}];

/// What the guest prints when its half passed.
const WROTE: &str = "read back as they were written";

/// What the guest prints when the machine has no writable disk.
const SKIPPED: &str = "btrfs-rw not checked";

/// Boot each architecture asked for, then check the volume it wrote.
///
/// # Errors
///
/// A boot that does not reach the marker, a guest check that did not run or
/// did not pass, a missing btrfs-progs, or anything `btrfs check` reports.
pub(crate) fn test_btrfs(
    args: &Args,
    build: impl Fn(Arch) -> Result<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<()> {
    let checker = Checker::required()?;
    for arch in args.arches()? {
        let (image, kernel) = build(arch)?;
        let written = qemu::test_btrfs_write(arch, &image, &kernel, args)?;
        if !written {
            return Err(Error::new(format!(
                "{arch}: the guest did not write the volume ({SKIPPED}), so there is nothing \
                 for btrfs check to judge"
            )));
        }
        checker.run(&btrfs_disk::blank_path(arch), arch)?;
    }
    Ok(())
}

/// Where `btrfs` is: on the path, or inside WSL.
pub(crate) enum Checker {
    Host,
    Wsl,
}

impl Checker {
    /// The checker, or the error that says to install one.
    ///
    /// # Errors
    ///
    /// No btrfs-progs on the host, nor in WSL.
    pub(crate) fn required() -> Result<Checker> {
        Checker::find().ok_or_else(|| {
            Error::new(
                "btrfs check is not on this host: install btrfs-progs (on Windows, in WSL), \
                 because the exit criterion is what it says about the volume Ferrix wrote",
            )
        })
    }

    fn find() -> Option<Checker> {
        if runs(Command::new("btrfs").arg("--version")) {
            return Some(Checker::Host);
        }
        if cfg!(windows) && runs(Command::new("wsl.exe").args(["btrfs", "--version"])) {
            return Some(Checker::Wsl);
        }
        None
    }

    /// Run `btrfs check` over `image`, failing with what it said.
    pub(crate) fn run(&self, image: &Path, arch: Arch) -> Result<()> {
        println!(
            "  {arch}: btrfs check --check-data-csum {}",
            image.display()
        );
        let mut command = match self {
            Checker::Host => {
                let mut command = Command::new("btrfs");
                let _ = command.arg("check");
                command
            }
            Checker::Wsl => {
                let mut command = Command::new("wsl.exe");
                let _ = command.args(["btrfs", "check"]);
                command
            }
        };
        let _ = command.args(["--readonly", "--check-data-csum"]);
        let _ = command.arg(wsl_path(self, image));
        let output = command
            .output()
            .map_err(|error| Error::new(format!("running btrfs check: {error}")))?;
        let said = String::from_utf8_lossy(&output.stderr).into_owned()
            + &String::from_utf8_lossy(&output.stdout);
        if !output.status.success() {
            return Err(Error::new(format!(
                "{arch}: btrfs check refused the volume Ferrix wrote:\n{}",
                said.trim()
            )));
        }
        // btrfs check exits zero having printed what it found in some
        // versions; a volume with errors says so in words as well.
        if said.contains("ERROR") || said.contains("errors found") {
            return Err(Error::new(format!(
                "{arch}: btrfs check found errors in the volume Ferrix wrote:\n{}",
                said.trim()
            )));
        }
        println!("  {arch}: btrfs check found nothing wrong");
        Ok(())
    }
}

/// The image's path as the checker sees it: WSL reaches a Windows path
/// through `/mnt/<drive>`.
fn wsl_path(checker: &Checker, image: &Path) -> String {
    let native = image.display().to_string();
    if !matches!(checker, Checker::Wsl) {
        return native;
    }
    let replaced = native.replace('\\', "/");
    match replaced.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => {
            format!("/mnt/{}/{rest}", drive.to_ascii_lowercase())
        }
        _ => replaced,
    }
}

/// Whether a command runs at all.
fn runs(command: &mut Command) -> bool {
    command.output().is_ok_and(|output| output.status.success())
}

/// Whether the guest's own half ran and passed, from its serial output.
pub(crate) fn guest_wrote(lines: &[String]) -> Result<bool> {
    if lines.iter().any(|line| line.contains(SKIPPED)) {
        return Ok(false);
    }
    if lines.iter().any(|line| line.contains(WROTE)) {
        return Ok(true);
    }
    Err(Error::new(
        "the boot said nothing about the writable btrfs disk, so its check did not run",
    ))
}
