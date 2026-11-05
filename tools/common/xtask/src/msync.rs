//! `cargo xtask test-msync`: what `msync(MS_SYNC)` said was on the disk is on the disk after
//! the power goes.
//!
//! Two boots of one writable volume, as `test-powerfail` cuts them. The guest
//! (`src/tests/msync`) writes a file through a shared mapping and msyncs it, and at the line
//! saying so QEMU is killed, with no chance to write anything more. A disk page written and not
//! written back goes within seconds by itself, so the cut comes at once. `btrfs check` judges the
//! volume, and the second boot reads the file.

use crate::args::Args;
use crate::btrfs_check::Checker;
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-msync",
    help_at: 181,
    help: &[
        "test-msync    Cut the power at msync(MS_SYNC)'s return; the next boot must read what it wrote",
    ],
    run: |_, args| test_msync(args),
}];

/// The guest program, `src/tests/msync`.
const TEST_PROGRAM: &str = "msync";
/// What boot 1 says as msync returns.
const SYNCED: &str = "msync: synced";
/// What boot 2 says when every word is there.
const SURVIVED: &str = "msync: survived";
/// What boot 2 says at a page that is not.
const LOST: &str = "msync: lost";
/// What either says when a call failed.
const FAILED: &str = "msync: FAILED";

/// Whether the second boot has said what it found.
fn said_a_verdict(lines: &[String]) -> bool {
    lines
        .iter()
        .any(|line| line.contains(SURVIVED) || line.contains(LOST) || line.contains(FAILED))
}

fn test_msync(args: &Args) -> Result<()> {
    let checker = Checker::required()?;
    for arch in args.arches()? {
        let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
        let loader = crate::cargo::build_loader(arch, args.release)?;
        let kernel = crate::cargo::build_kernel_with_init(
            arch,
            args.release,
            &program,
            crate::shell::SCRIPT,
        )?;
        let natives = crate::native::build(arch, args.release)?;
        let archive = crate::initramfs::build(None, &natives, None, &[])?;
        let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
        btrfs_disk::keep_blank(false);
        let first = qemu::watch_then(arch, &image, &kernel, args, SYNCED, |watching| {
            watching.cut_power();
            Ok(())
        });
        let second = first.and_then(|_| {
            checker.run(&btrfs_disk::blank_path(arch), arch)?;
            btrfs_disk::keep_blank(true);
            // Stage 12's own check would write over what boot 1 left.
            let reader = crate::fat::write_image_with(
                arch,
                &loader,
                &kernel,
                &archive,
                Some("ferrix.btrfs=keep\n"),
            )?;
            qemu::watch_then(arch, &reader, &kernel, args, "msync: started", |watching| {
                let _ = watching.read_more(
                    std::time::Instant::now() + std::time::Duration::from_secs(30),
                    said_a_verdict,
                )?;
                watching.stop_when_done();
                Ok(())
            })
        });
        btrfs_disk::keep_blank(false);
        let lines = second?;
        if !lines.iter().any(|line| line.contains(SURVIVED)) {
            let said = lines
                .iter()
                .find(|line| line.contains(LOST) || line.contains(FAILED));
            return Err(Error::new(format!(
                "{arch}: test-msync: what msync(MS_SYNC) said was written is not on the disk: {}",
                said.map_or("no verdict from the second boot", String::as_str)
            )));
        }
        println!("  {arch}: test-msync: what msync(MS_SYNC) wrote survived the power");
    }
    Ok(())
}
