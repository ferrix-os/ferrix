//! `test-game-rig`: stage 24's R3 on Ferrix, the half that needs no card
//! (`docs/GAME-PATH.md` §2.4 and §8.1).
//!
//! The rig's load, its sleep and wake-up program and its clock are judged
//! on the machine Ferrix runs on under KVM: the same `tools/common/game/`
//! files the Linux guest runs (`linux-guest.sh`), here carried in the
//! initramfs, with the rustc volume `test-rustc` attaches for the compiler
//! the load runs. The kernel's `clock` line must name the TSC, which
//! `qemu::x86_cpu` asks for with `+invtsc` under KVM; and while the load
//! runs, `rig busy` must find every processor at least [`BUSY`] percent
//! busy. The overshoot of a timed sleep, idle and under the load, is
//! printed and not judged: its bound is G2's (§8.3), not the rig's.
//!
//! The controls (`docs/GAME-PATH.md` §12 records both runs):
//! `FERRIX_X86_CPU` set to [`qemu::x86_cpu`]'s model without `+invtsc`
//! makes the clock the HPET and fails the clock check; and `load.sh` edited
//! to start no compile loop leaves the processors idle and fails the busy
//! check.

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, cargo, fat, initramfs, native, ports, qemu, rustc, shell, zinc};
use std::process::Command;

/// The script the shell runs: `measure.sh`, as the Linux guest runs it,
/// with the compiler on the volume and the writer's file on its disk.
const SCRIPT: &str = r#"export PATH=/data/rust/bin:/bin
export RIG=/bin/rig
export RIG_RUSTC=/data/rust/bin/rustc
export RIG_HERE=/usr/share/game
export RIG_CRATE=/usr/share/game/load-crate.rs
export RIG_WORK=/tmp/rig-load
export RIG_DISK=/data
export RIG_WRITE_MIB=256
sh /usr/share/game/measure.sh 30 || exit $?
exit 16
"#;

/// The status the script exits with when `measure.sh` got to its end.
const STATUS: &str = "16";

/// The lowest busy share any processor may show under the load, in
/// percent. Short of 100 by the ticks a compile loop spends between two
/// compiler runs.
const BUSY: f64 = 99.0;

/// The files `tools/common/game/` gives the guest, and where.
const CARRIED: &[(&str, &str)] = &[
    ("usr/share/game/load.sh", "load.sh"),
    ("usr/share/game/measure.sh", "measure.sh"),
    ("usr/share/game/load-crate.rs", "load-crate.rs"),
];

/// Guest memory unless `--memory` says otherwise: four compilers at about
/// 300 MiB each, with room above the holder's 80 percent.
const MEMORY: u32 = 8192;

/// Processors unless `--smp` says otherwise.
const SMP: u32 = 4;

/// Seconds to wait unless `--timeout` says otherwise.
const TIMEOUT: u64 = 900;

/// `tools/common/game/rig.c`, built static with the host's compiler: the
/// binary the Linux guest runs too.
fn build_rig() -> Result<Vec<u8>> {
    let source = crate::paths::workspace_root().join("tools/common/game/rig.c");
    let dir = crate::paths::build_dir(Arch::X86_64);
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    let out = dir.join("rig");
    let built = Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-static", "-o"])
        .arg(&out)
        .arg(&source)
        .status()
        .map_err(|error| Error::new(format!("running cc: {error}")))?;
    if !built.success() {
        return Err(Error::new(format!(
            "building {} failed ({built}); it needs the host's static libc",
            source.display()
        )));
    }
    std::fs::read(&out).map_err(|error| Error::new(format!("reading {}: {error}", out.display())))
}

/// The rig's binary and scripts, and the links the compiler needs.
fn files() -> Result<Vec<ports::File>> {
    let dir = crate::paths::workspace_root().join("tools/common/game");
    let mut files = rustc::files(rustc::LINKS);
    files.push(ports::File {
        path: "bin/rig".to_owned(),
        mode: 0o755,
        content: ports::Content::Bytes(build_rig()?),
    });
    for (path, name) in CARRIED {
        let from = dir.join(name);
        let bytes = std::fs::read(&from)
            .map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
        files.push(ports::File {
            path: (*path).to_owned(),
            mode: 0o755,
            content: ports::Content::Bytes(bytes),
        });
    }
    Ok(files)
}

/// Boot the rig's load on Ferrix and judge the clock and every processor.
///
/// # Errors
///
/// When `--init` (a static busybox) or the volume is missing, the image
/// cannot be built, the boot fails, or either check fails.
pub(crate) fn test_game_rig(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-game-rig runs on x86-64 only: the volume holds x86-64 binaries",
        ));
    }
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new("test-game-rig needs --init PATH, a static busybox: the scripts run its applets")
    })?;
    let mut args = args.clone();
    args.data_image = Some(rustc::volume()?);
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.smp_given {
        args.smp = SMP;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    let busybox = crate::program_for(init, arch)?;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files()?)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    println!(
        "  {arch}: the rig's load on {} processors and {} MiB (timeout {}s)",
        args.smp, args.memory, args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    judge(arch, args.smp, &lines)
}

/// The kernel's `clock` line: the counter it chose.
fn clock_line(lines: &[String]) -> Option<&str> {
    lines.iter().find_map(|line| {
        let rest = line.trim_start().strip_prefix("clock    ")?;
        (rest.contains(" at ") && !rest.contains("seconds since")).then_some(rest)
    })
}

/// `rig busy`'s summary: how many processors, and the lowest share.
fn busy(lines: &[String]) -> Option<(u32, f64)> {
    lines.iter().find_map(|line| {
        let rest = line.trim().strip_prefix("busy: ")?;
        let (count, rest) = rest.split_once(" processors over ")?;
        let lowest = rest.split_once(", lowest ")?.1.split_once('%')?.0;
        Some((count.parse().ok()?, lowest.parse().ok()?))
    })
}

/// Whether the clock is the TSC, the script ran to its end, and every
/// processor was busy under the load; the rig's figures printed either way.
fn judge(arch: Arch, smp: u32, lines: &[String]) -> Result<()> {
    for line in lines.iter().map(|line| line.trim()) {
        let shown = [
            "wake: ",
            "busy: ",
            "hold: ",
            "write: ",
            "load: ",
            "compile: ",
            "launch: ",
        ];
        if shown.iter().any(|prefix| line.starts_with(prefix)) {
            println!("  {arch}: {line}");
        }
    }
    let clock = clock_line(lines).unwrap_or("missing");
    println!("  {arch}: the clock line: {clock}");
    let exited = lines
        .iter()
        .find_map(|line| line.trim().strip_prefix(shell::EXITED))
        .map(str::trim);
    if !clock.starts_with("TSC") {
        return Err(Error::new(format!(
            "{arch}: the clock is not the TSC: `{clock}`"
        )));
    }
    if exited != Some(STATUS) {
        return Err(Error::new(format!(
            "{arch}: measure.sh did not get to its end (the shell exited with {})",
            exited.unwrap_or("nothing")
        )));
    }
    match busy(lines) {
        Some((count, lowest)) if count == smp && lowest >= BUSY => {
            println!(
                "  {arch}: the load held all {count} processors, the least busy at {lowest}% \
                 (at least {BUSY}%), and the clock is the TSC"
            );
            Ok(())
        }
        Some((count, lowest)) => Err(Error::new(format!(
            "{arch}: under the load {count} of {smp} processors were counted and the least \
             busy was at {lowest}%, under {BUSY}%"
        ))),
        None => Err(Error::new(format!("{arch}: `rig busy` printed no summary"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn the_clock_line_is_found_and_the_epoch_line_is_not() {
        let seen = lines(
            "  clock    1789246016 seconds since the epoch, from firmware's clock\n\
             \x20 clock    TSC, calibrated against the HPET at 4400.000 MHz",
        );
        assert_eq!(
            clock_line(&seen),
            Some("TSC, calibrated against the HPET at 4400.000 MHz")
        );
        assert_eq!(
            clock_line(&lines("  clock    HPET at 100.000 MHz")),
            Some("HPET at 100.000 MHz")
        );
    }

    #[test]
    fn the_busy_summary_parses() {
        let seen = lines("busy: 8 processors over 10 s, lowest 99.5% (cpu2), mean 99.8%");
        assert_eq!(busy(&seen), Some((8, 99.5)));
        assert_eq!(
            busy(&lines("busy: cpu0 99.8% of 1000 ticks, steal 1")),
            None
        );
    }

    #[test]
    fn a_clock_other_than_the_tsc_fails() {
        let seen = lines(
            "  clock    HPET at 100.000 MHz\n\
             busy: 4 processors over 10 s, lowest 100.0% (cpu0), mean 100.0%\n\
             init     the shell exited with 16",
        );
        assert!(judge(Arch::X86_64, 4, &seen).is_err());
    }

    #[test]
    fn an_idle_processor_fails() {
        let seen = lines(
            "  clock    TSC, calibrated against the HPET at 4400.000 MHz\n\
             busy: 4 processors over 10 s, lowest 3.0% (cpu3), mean 76.0%\n\
             init     the shell exited with 16",
        );
        assert!(judge(Arch::X86_64, 4, &seen).is_err());
        let held = lines(
            "  clock    TSC, calibrated against the HPET at 4400.000 MHz\n\
             busy: 4 processors over 10 s, lowest 99.6% (cpu3), mean 99.9%\n\
             init     the shell exited with 16",
        );
        assert!(judge(Arch::X86_64, 4, &held).is_ok());
    }
}
