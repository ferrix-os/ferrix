//! `test-sud`: Syscall User Dispatch as Proton's Wine uses it (`docs/SECCOMP.md`
//! §13, the design's 7.2).
//!
//! The program is `src/tests/sud/`, built for x86-64's musl target and for
//! 32-bit x86, which the kernel runs in compatibility mode, and booted as init
//! on the x86-64 kernel: the only architecture that offers dispatch, as on
//! Linux. It names its own text as the range whose calls run, keeps a selector
//! byte, and makes calls from a page at 0x600000000000, as DW-Proton's
//! self-test maps it: allowed, dispatched with the `SIGSYS` and context Wine's
//! handler reads and writes, a restart code and a native-range number, an
//! `int $0x80`, kills, prctl's refusals, a thread, a fork child and an exec
//! that start without it, and a seccomp filter judged after it. It prints a
//! line per step and `sud: all ok`, and exits 0; this requires every one of
//! those lines, in order, and the status.
//!
//! Then two negative controls, as Cargo features of the same program: a
//! handler that leaves the result out, and a selector never set to BLOCK. Each
//! must fail on the block step.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::sem::{build_test, says, status};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell};

/// Where the image carries the program as a file too, for its exec step. The
/// guest program names the same path.
const CARRIED_AT: &str = "/bin/sud-test";

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "sud: setup ok",
    "sud: allow ok",
    "sud: block ok",
    "sud: context ok",
    "sud: special ok",
    "sud: compat ok",
    "sud: blocked ok",
    "sud: kills ok",
    "sud: prctl ok",
    "sud: inherit ok",
    "sud: seccomp ok",
    "sud: nt ok",
    "sud: all ok",
];

/// A negative control: the Cargo feature, the line it must fail with, and the
/// step that must not have passed.
struct Control {
    /// The feature of `sud-test` that sabotages it.
    feature: &'static str,
    /// The start of the failure line.
    failure: &'static str,
    /// The step that must not have passed.
    step: &'static str,
}

/// The negative controls.
const CONTROLS: &[Control] = &[
    Control {
        feature: "negative-control-rax",
        failure: "sud: FAILED block: a dispatched call did not return the result its handler wrote",
        step: "sud: block ok",
    },
    Control {
        feature: "negative-control-allow",
        failure: "sud: FAILED block: a call from outside the range under BLOCK was not dispatched",
        step: "sud: block ok",
    },
];

/// The test on x86-64 and on 32-bit x86, or on 32-bit x86 alone with `--i686`.
pub(crate) fn test_sud(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let abis: &[bool] = if args.i686 { &[true] } else { &[false, true] };
    for &i686 in abis {
        let name = if i686 { "i386 on x86_64" } else { "x86_64" };
        let log = paths::build_dir(arch).join("serial.log");
        let program = build(arch, None, i686)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("sud: FAILED"));
                return Err(Error::new(format!(
                    "{name}: the sud test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{name}: the sud test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {name}: allowed and dispatched calls, the SIGSYS and the context Wine's handler \
             reads and writes, a restart code, the native range and int $0x80, kills, prctl's \
             refusals, nothing inherited and seccomp after it, exit 0"
        );
        // The controls on the 64-bit build only: they sabotage the program,
        // not the ABI.
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

/// Build `sud-test`, with `feature` on if there is one.
fn build(arch: Arch, feature: Option<&str>, i686: bool) -> Result<PathBuf> {
    build_test("sud", arch, feature, i686)
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
        path: CARRIED_AT.trim_start_matches('/').to_owned(),
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
