//! `cargo xtask test-pty`: a program on a pseudoterminal, and what it wrote
//! read back through the master.
//!
//! `docs/ROADMAP.md` stage 18's exit asks for a terminal, and a terminal is a
//! program holding one end of a pseudoterminal with another program on the
//! other. This is the pair without the window: the term app's `term --headless`
//! opens `/dev/ptmx`, asks it which pair it is, unlocks it, opens
//! `/dev/pts/<n>`, forks, gives the child the slave for its session and its
//! three descriptors, and runs it. What the child writes comes back through
//! the master, goes through the terminal's own grid -- the same one the
//! window draws -- and is printed a row at a time.
//!
//! The child is `/bin/hyprctl` with no arguments, which prints its usage and
//! stops. A program that needs nothing of the machine but a terminal to
//! write on: what is being tested is the pseudoterminal, not the program.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::qemu::Watching;
use crate::{Error, Result};

/// What the terminal prints once the program has finished.
const MARKER: &str = "term: ";

/// Where the program the terminal runs goes in the initramfs.
const CTL_PATH: &str = "bin/hyprctl";

/// How long to wait for the lines after the terminal has started.
const PATIENCE: Duration = Duration::from_secs(30);

/// The line the child prints, which must come back through the pair and be
/// drawn in the grid.
const WANTED: &str = "term: | hyprctl: usage:";

/// Build a compositor program for `arch`, and say where it is.
fn build(arch: Arch, package: &str, binary: &str) -> Result<PathBuf> {
    let target = crate::display::target(arch).ok_or_else(|| {
        Error::new(format!(
            "{arch} has no target for the compositor's programs; the pseudoterminal test runs on \
             x86_64 and aarch64"
        ))
    })?;
    let target_dir = paths::target_dir().join("compositor").join("term");
    println!("  building src/user/system/linux/compositor/{binary} for {target}");
    let program = target_dir.join(target).join("release").join(binary);
    crate::builds::Build::cargo(
        format!("cargo build (src/user/system/linux/compositor/{binary}) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/compositor"),
    )
    .args(["build", "--release", "-p", package, "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program)
    .run()?;
    Ok(program)
}

/// Boot the terminal as init, with the program it runs in the initramfs.
fn boot(arch: Arch, term: &Path, ctl: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = crate::cargo::build_loader(arch, args.release)?;
    // One argument a line, as `Options::unshell` reads them.
    let script = format!("--headless\n/{CTL_PATH}");
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, term, &script)?;
    let natives = crate::native::build(arch, args.release)?;
    let carried = [crate::ports::File {
        path: CTL_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(
            std::fs::read(ctl)
                .map_err(|error| Error::new(format!("reading {}: {error}", ctl.display())))?,
        ),
    }];
    let initramfs = crate::initramfs::build(None, &natives, None, &carried)?;
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;

    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        // Both lines: the program's own, drawn in the grid, and the
        // terminal's last, which says how much came back. The last is the
        // one to wait for, since it is printed after the other.
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(WANTED))
                && lines.iter().any(|line| line.contains(" wrote "))
        })?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, args, MARKER, hook)?;
    Ok(said)
}

/// `test-pty` on each architecture asked for.
///
/// # Errors
///
/// A guest whose pseudoterminal did not carry the program's output.
pub(crate) fn test_pty(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if crate::display::target(arch).is_none() {
            println!("  {arch}: no target for the compositor's programs; skipped");
            continue;
        }
        let term = crate::apps::program(arch, "term", "term")?;
        let ctl = build(arch, "compositor-ctl", "hyprctl")?;
        let said = boot(arch, &term, &ctl, args)?;
        let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
        if !has(WANTED) {
            return Err(Error::new(format!(
                "{arch}: nothing the program wrote came back through the pseudoterminal; the \
                 guest said:\n    {}",
                said.join("\n    ")
            )));
        }
        // And the bytes were counted, which says the master read them rather
        // than the grid having been filled some other way.
        // The terminal's own line, not the boot report's: `xtask wrote them`
        // is a line the block driver prints, and it holds no number.
        let wrote = said
            .iter()
            .filter(|line| line.contains("term: "))
            .find_map(|line| line.split(" wrote ").nth(1))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|number| number.parse::<usize>().ok())
            .unwrap_or(0);
        if wrote == 0 {
            return Err(Error::new(format!(
                "{arch}: the master read no bytes at all; the guest said:\n    {}",
                said.join("\n    ")
            )));
        }
        println!(
            "  {arch}: a program on `/dev/pts/0` wrote {wrote} bytes, and the terminal read them \
             through `/dev/ptmx` and drew them"
        );
    }
    Ok(())
}
