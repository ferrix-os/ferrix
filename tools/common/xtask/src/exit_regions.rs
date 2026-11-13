//! `test-exit-regions`: a process of tens of thousands of regions ended by
//! `exit` and by `SIGKILL`, with the machine still up after both, and the
//! same count of regions made in one reused object.
//!
//! Boots `src/tests/exit-regions` as init (its head says what it does).
//! The N150 box reset itself on 2026-11-12 when such a process ended: the
//! ending task gives its whole address space back in one unmap on its own
//! kernel stack, and a sort by region count on that unmap recursed past the
//! stack's end (`docs/certification/FINDINGS.md`). The row fails on any
//! stop of the kernel, on a phase that did not finish, and on a status
//! other than 0.
//!
//! **The reused-object phase** (ledger 9561 N8): its child's regions are
//! `MAP_FIXED` views of one reserve, which anon-merge makes in one object
//! (`docs/ANON-MERGE.md` 5.5); the row requires its line of the reserve's
//! maps lines, its figures, its memory line and its `reused ok`, failing on
//! any missing, and prints its teardown time beside the plain exit's.
//!
//! **Teardown times: reporting, not asserted.** Each phase's time from the
//! end to `waitpid`'s return and its canary's worst lateness are printed
//! for the record; the teardown in pieces that would bound them is a design
//! of its own (`docs/OOM-REAPER.md`).

use std::path::Path;

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, paths, qemu, sem, shell};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-exit-regions",
    help_at: 576,
    help: &[
        "test-exit-regions  Boot exit-regions-test as init: a child of 60,000 regions (40,000 on Arm) ended by exit,",
        "              then by SIGKILL, by SIGPIPE, and one in one reused object; require the kernel to give each back and stay up",
    ],
    run: |_, args| test(args),
}];

/// Processors unless told otherwise: a canary beside the teardown.
const PROCESSORS: u32 = 4;
/// Memory, in MiB: one child's 940 MiB and room.
const MEMORY: u32 = 2048;
/// Seconds the boot may take.
const TIMEOUT: u64 = 600;
/// The phases, as the program names them.
const PHASES: [&str; 3] = ["exit", "kill", "sigpipe"];

/// Boot the program on every architecture asked for.
///
/// Verifies: L.user.247
///
/// # Errors
///
/// A build or boot that fails, a kernel stop, a phase that printed no
/// figures, or a status other than 0.
fn test(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.smp_given {
        args.smp = PROCESSORS;
    }
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    for arch in args.arches()? {
        let program = sem::build_test("exit-regions", arch, None, false)?;
        let log = paths::build_dir(arch).join("serial.log");
        let lines = boot(arch, &program, &args)?;
        if let Some(failed) = lines
            .iter()
            .find(|line| line.contains("exitregions: FAILED"))
        {
            return Err(Error::new(format!(
                "{arch}: `{}`.\n  Serial output is in {}",
                failed.trim(),
                log.display()
            )));
        }
        for phase in PHASES {
            let prefix = format!("exitregions: {phase} regions ");
            let Some(line) = lines.iter().find(|line| line.contains(&prefix)) else {
                return Err(Error::new(format!(
                    "{arch}: phase {phase} printed no figures.\n  Serial output is in {}",
                    log.display()
                )));
            };
            let figures = line
                .find("exitregions: ")
                .and_then(|at| line.get(at..))
                .unwrap_or(line);
            println!("  {arch}: {figures} (reporting, not asserted)");
        }
        reused(arch, &lines, &log)?;
        // The enomem phase: a child mapped until refused, its memory back
        // while a zombie and at `waitpid`'s return (F-114, L.user.247).
        for want in [
            "exitregions: enomem: the child was refused at region ",
            "exitregions: enomem guest ",
        ] {
            let Some(line) = lines.iter().find(|line| line.contains(want)) else {
                return Err(Error::new(format!(
                    "{arch}: the enomem phase printed no `{want}` line.\n  Serial output is in {}",
                    log.display()
                )));
            };
            let figures = line
                .find("exitregions: ")
                .and_then(|at| line.get(at..))
                .unwrap_or(line);
            println!("  {arch}: {figures}");
        }
        if !lines
            .iter()
            .any(|line| sem::says(line, "exitregions: enomem ok"))
        {
            return Err(Error::new(format!(
                "{arch}: the enomem phase's memory was not back by its child's reap.\n  Serial \
                 output is in {}",
                log.display()
            )));
        }
        if !lines
            .iter()
            .any(|line| sem::says(line, "exitregions: done"))
            || sem::status(&lines) != Some(0)
        {
            return Err(Error::new(format!(
                "{arch}: exit-regions-test did not finish with status 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: every child given back, the kernel still up");
    }
    Ok(())
}

/// The reused-object phase's lines: the same count of regions made as
/// views of one reserve, all one object's (ledger 9561 N8), ended by
/// `_exit`, its memory back at `waitpid`'s return. Each line is required;
/// its teardown is printed beside the plain exit's.
fn reused(arch: Arch, lines: &[String], log: &Path) -> Result<()> {
    let mut reused_figures = Vec::new();
    for want in [
        "exitregions: reused: the reserve shows ",
        "exitregions: reused regions ",
        "exitregions: reused guest ",
    ] {
        let Some(line) = lines.iter().find(|line| line.contains(want)) else {
            return Err(Error::new(format!(
                "{arch}: the reused-object phase printed no `{want}` line.\n  Serial output \
                 is in {}",
                log.display()
            )));
        };
        let figures = line
            .find("exitregions: ")
            .and_then(|at| line.get(at..))
            .unwrap_or(line);
        println!("  {arch}: {figures}");
        reused_figures.push(figures);
    }
    if !lines
        .iter()
        .any(|line| sem::says(line, "exitregions: reused ok"))
    {
        return Err(Error::new(format!(
            "{arch}: the reused-object phase's memory was not back at its child's reap.\n  \
             Serial output is in {}",
            log.display()
        )));
    }
    let plain = lines
        .iter()
        .find(|line| line.contains("exitregions: exit regions "))
        .and_then(|line| teardown_ms(line));
    let reused = reused_figures.get(1).and_then(|line| teardown_ms(line));
    let (Some(plain), Some(reused)) = (plain, reused) else {
        return Err(Error::new(format!(
            "{arch}: no teardown time on the exit or the reused-object phase's line.\n  \
             Serial output is in {}",
            log.display()
        )));
    };
    println!(
        "  {arch}: teardown by exit, {reused} ms in one reused object beside {plain} ms in \
         one object each (reporting, not asserted)"
    );
    Ok(())
}

/// The milliseconds a phase's figures line gives after `teardown`.
fn teardown_ms(line: &str) -> Option<u64> {
    let (_, after) = line.split_once(" teardown ")?;
    after.split_whitespace().next()?.parse().ok()
}

/// Where the program is carried in the image, for its children to run: a
/// fresh image of it has a new process's regions, as the box's had, where a
/// fork's copy keeps its parent's.
const CARRIED: &str = "bin/exit-regions-test";

/// [`sem::boot`], with the program also carried at [`CARRIED`].
fn boot(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    let carried = [File {
        path: CARRIED.to_owned(),
        mode: 0o755,
        content: Content::Bytes(bytes),
    }];
    let archive = initramfs::build(None, &natives, None, &carried)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
    let after_boot = lines
        .iter()
        .position(|line| qemu::reached(line, qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
        .to_vec();
    Ok(after_boot)
}
