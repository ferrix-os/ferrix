//! `test-seccomp`: seccomp-bpf as a program uses it (`docs/SECCOMP.md` §8.2).
//!
//! The program is `src/tests/seccomp/`, built for each architecture's musl
//! target -- and, on x86-64, for 32-bit x86 as well, which the kernel runs in
//! compatibility mode through `int $0x80`, so that one run of `--arch all`
//! covers the four ABIs -- and booted as init. It installs filters through raw
//! `seccomp(2)` and `prctl(2)` calls with hand-written classic-BPF programs, as
//! Chromium, bubblewrap and Flatpak do: the probes Chromium makes, errnos and
//! the order of a chain, an unprivileged install, `/proc/self/status`, one
//! filter for every ABI, Chromium's `SIGSYS` handler emulating a trapped call
//! from its context, a trap with `SIGSYS` ignored, `KILL_PROCESS` and
//! `KILL_THREAD`, strict mode, bubblewrap's `execve`, and `TSYNC` among threads
//! that are being made. It prints a line per step and `seccomp: all ok`, and
//! exits 0; this requires every one of those lines, in order, and the status.
//!
//! Then two negative controls, as Cargo features of the same program: the
//! handler that leaves the trapped call's result out must fail on the emulation
//! step, and the unprivileged step that sets no-new-privs first must fail on
//! that step. A check that could not fail would pass those builds too.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::sem::{build_test, says, status};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell};

/// Where the image carries the program as a file too: booted as init it is
/// built into the kernel and has no file of its own, and bubblewrap's step
/// `execve`s it. The guest program names the same path.
const CARRIED_AT: &str = "/bin/seccomp-test";

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "seccomp: probes ok",
    "seccomp: errno ok",
    "seccomp: order ok",
    "seccomp: unprivileged ok",
    "seccomp: status ok",
    "seccomp: twoarch ok",
    "seccomp: emulate ok",
    "seccomp: ignored ok",
    "seccomp: kill ok",
    "seccomp: strict ok",
    "seccomp: bwrap ok",
    "seccomp: tsync ok",
    "seccomp: all ok",
];

/// A negative control: the Cargo feature, the line it must fail with, and the
/// last step that must have passed before it.
struct Control {
    /// The feature of `seccomp-test` that sabotages it.
    feature: &'static str,
    /// The start of the failure line.
    failure: &'static str,
    /// The step that must not have passed.
    step: &'static str,
}

/// The negative controls.
const CONTROLS: &[Control] = &[
    Control {
        feature: "negative-control-trap",
        failure: "seccomp: FAILED emulate: a trapped call did not return the result its handler wrote",
        step: "seccomp: emulate ok",
    },
    Control {
        feature: "negative-control-privilege",
        failure: "seccomp: FAILED unprivileged: an unprivileged install without no-new-privs was not refused EACCES",
        step: "seccomp: unprivileged ok",
    },
];

/// Build `seccomp-test` for `arch`, with `feature` on if there is one.
fn build(arch: Arch, feature: Option<&str>, i686: bool) -> Result<PathBuf> {
    build_test("seccomp", arch, feature, i686)
}

/// The ABIs `--arch` asks for: each architecture's own, and on x86-64 also
/// 32-bit x86 unless `--i686` already says which.
fn abis(args: &Args) -> Result<Vec<(Arch, bool)>> {
    let mut abis = Vec::new();
    for arch in args.arches()? {
        abis.push((arch, args.i686));
        if arch == Arch::X86_64
            && !args.i686
            && matches!(args.arch.as_deref(), Some("both" | "all"))
        {
            abis.push((arch, true));
        }
    }
    Ok(abis)
}

/// The test on every ABI asked for.
pub(crate) fn test_seccomp(args: &Args) -> Result<()> {
    for (arch, i686) in abis(args)? {
        let name = if i686 {
            "i386 on x86_64".to_string()
        } else {
            arch.to_string()
        };
        let log = paths::build_dir(arch).join("serial.log");

        let program = build(arch, None, i686)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("seccomp: FAILED"));
                return Err(Error::new(format!(
                    "{name}: the seccomp test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{name}: the seccomp test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {name}: probes, errnos, chain order, the unprivileged rule, status, a filter for \
             every ABI, a trapped call emulated from its context, kills, strict mode, execve and \
             TSYNC among threads being made, exit 0"
        );

        // The controls on the architecture's own ABI only: they sabotage the
        // program, not the ABI, and a 32-bit build adds a boot and nothing else.
        if i686 {
            continue;
        }
        for control in CONTROLS {
            let sabotaged = build(arch, Some(control.feature), false)?;
            let lines = boot(arch, &sabotaged, args)?;
            let failed_there = lines.iter().any(|line| line.contains(control.failure));
            let passed_it = lines.iter().any(|line| says(line, control.step));
            if !failed_there || passed_it || status(&lines) != Some(1) {
                return Err(Error::new(format!(
                    "{name}: the negative control `{}` should fail with `{}` and exit 1, and \
                     did not.\n  Serial output is in {}",
                    control.feature,
                    control.failure,
                    log.display()
                )));
            }
            println!(
                "  {name}: the negative control `{}` failed as it must",
                control.feature
            );
        }
    }
    Ok(())
}

/// Boot `program` as init, with the image carrying it at [`CARRIED_AT`] as
/// well, and return the serial lines from the kernel's success marker on.
fn boot(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let bytes = std::fs::read(program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let carried = [File {
        // The archive's paths are relative to its root.
        path: CARRIED_AT.trim_start_matches('/').to_string(),
        mode: 0o755,
        content: Content::Bytes(bytes),
    }];
    let archive = initramfs::build(None, &natives, None, &carried)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
    Ok(lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
        .to_vec())
}
