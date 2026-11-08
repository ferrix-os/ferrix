//! `test-threads`: stage 7's threads exit test.
//!
//! "A static musl Rust program that uses `std::thread`, `Mutex` and `mpsc`
//! runs under `test-shell` on all three architectures." The program is
//! `src/tests/threads/`, built here for each architecture's musl target and booted
//! as init. It prints a line per step and `threads: all ok`, and exits 0; this
//! requires every one of those lines, in order, and the status.
//!
//! Its last step is the one stage 7 owed after its exit: credentials are kept
//! per process, a written deviation from Linux, and musl's `set*id` repeats
//! the call on every thread after the first has already changed the ids. The
//! program calls `setuid(1000)` as root with a second thread alive, and that
//! survives only while setting an id to a current one is permitted; a refused
//! repeat makes musl kill the process, which the program names. After it, a
//! timed `FUTEX_WAIT` that a handled `SIGUSR1` interrupts must end `EINTR`
//! at once, under `SA_RESTART` too, as Linux's `futex_wait` answers, and one
//! stopped and continued must resume to its own deadline.
//!
//! Then the negative control: the same program built to expect one thread more
//! under `/proc/self` than it has must fail, on the `/proc` step and for that
//! reason, and still end with the status it chose. A check of the thread count
//! that could not fail would pass that build too.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, native, qemu, shell};

/// The Rust target the program is built for on `arch`: with `i686`, x86-64's
/// program is 32-bit x86, which the kernel runs in compatibility mode through
/// `int $0x80` (`docs/I386.md`, I4).
fn target(arch: Arch, i686: bool) -> Result<&'static str> {
    Ok(match (arch, i686) {
        (Arch::X86_64, false) => "x86_64-unknown-linux-musl",
        (Arch::X86_64, true) => "i686-unknown-linux-musl",
        (Arch::AArch64, false) => "aarch64-unknown-linux-musl",
        (Arch::Armv7a, false) => "armv7-unknown-linux-musleabi",
        (_, true) => {
            return Err(Error::new(format!(
                "--i686 is a program for the x86-64 kernel, not for {arch}"
            )));
        }
    })
}

/// The lines a working run prints, in order.
const STEPS: &[&str] = &[
    "threads: armed its-own-number ok",
    "threads: armed reparent ok",
    "threads: armed reparent-working ok",
    "threads: armed stop-kill ok",
    "threads: armed exit-group ok",
    "threads: armed ok",
    "threads: spawn ok",
    "threads: proc ok",
    "threads: channel ok",
    "threads: join ok",
    "threads: mutex ok",
    "threads: copy ok",
    "threads: munmap-race ok",
    "threads: setuid ok",
    "threads: futex-signal ok",
    "threads: futex-stop ok",
    "threads: cpu-number ok",
    "threads: slice-request ok",
    "threads: affinity ok",
    "threads: all ok",
];

/// The line the negative control must fail with.
const NEGATIVE_FAILURE: &str = "threads: FAILED /proc/self/status does not count every thread";

/// What a `threads-test` build is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    /// Every step.
    Plain,
    /// The negative control's: one thread too many expected.
    Negative,
    /// The stall-probe step alone (`cargo xtask stall-probe`).
    StallProbe,
}

/// Build `threads-test` for `arch`, as the negative control or not, and return
/// where the program is.
fn build(arch: Arch, negative: bool, i686: bool) -> Result<PathBuf> {
    build_flavour(
        arch,
        if negative {
            Flavour::Negative
        } else {
            Flavour::Plain
        },
        i686,
    )
}

/// [`build`] for any [`Flavour`].
fn build_flavour(arch: Arch, flavour: Flavour, i686: bool) -> Result<PathBuf> {
    let target = target(arch, i686)?;
    let (flavour, feature) = match flavour {
        Flavour::Plain => ("plain", None),
        Flavour::Negative => ("negative", Some("negative-control")),
        Flavour::StallProbe => ("stall-probe", Some("stall-probe")),
    };
    let target_dir = paths::target_dir().join("threads-test").join(flavour);
    println!("  building threads-test ({flavour}) for {target}");
    let program = target_dir.join(target).join("release").join("threads-test");
    let mut build = crate::builds::Build::cargo(
        format!("cargo build (threads-test, {flavour}) --target {target}"),
        paths::workspace_root().join("src/tests/threads"),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program);
    if let Some(feature) = feature {
        build = build.args(["--features", feature]);
    }
    // Ferrix's own `.cargo/config.toml` uses this triple for the ARMv7-A
    // loader, with its linker script, and cargo merges a parent directory's
    // flags for a target into `src/tests/threads/`'s. The variable replaces every
    // configured flag, so the program is linked as the other two are.
    if arch == Arch::Armv7a {
        build = build
            .env(
                "CARGO_TARGET_ARMV7_UNKNOWN_LINUX_MUSLEABI_LINKER",
                "rust-lld",
            )
            .env(
                "RUSTFLAGS",
                "-C linker-flavor=ld.lld -C link-self-contained=yes \
                 -C target-feature=+crt-static",
            );
    }
    build.run()?;
    Ok(program)
}

/// Boot `program` as init on `arch` and return every line after the boot
/// marker, once init has exited.
fn boot(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let image = fat::write_image(arch, &loader, &kernel, &natives, None)?;
    let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
    let after_boot = lines
        .iter()
        .position(|line| qemu::reached(line, qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
        .to_vec();
    Ok(after_boot)
}

/// The status init exited with, from the kernel's line.
fn status(lines: &[String]) -> Option<i32> {
    lines.iter().find_map(|line| {
        let at = line.find(shell::EXITED)?;
        line.get(at + shell::EXITED.len()..)?.trim().parse().ok()
    })
}

/// Whether `line` is `want`, once the serial log's time stamp is taken off.
fn says(line: &str, want: &str) -> bool {
    line.trim_end().ends_with(want)
}

/// The test on every architecture asked for.
///
/// Verifies: `L.sched.109`
/// Verifies: `L.sched.99`, `L.sched.100`, `L.sched.101`
pub(crate) fn test_threads(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");

        // Every build before the first boot, so a plan run, which stops at
        // the first boot, records the negative control too.
        let program = build(arch, false, args.i686)?;
        let negative = build(arch, true, args.i686)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("threads: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the threads test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the threads test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {arch}: std::thread, Mutex and mpsc ran, /proc/self counted every thread, \
             two threads survived musl's setuid(1000) broadcast, a handled signal ended a \
             timed futex wait EINTR, exit 0"
        );

        let lines = boot(arch, &negative, args)?;
        let failed_there = lines.iter().any(|line| says(line, NEGATIVE_FAILURE));
        let passed_proc = lines.iter().any(|line| says(line, "threads: proc ok"));
        if !failed_there || passed_proc || status(&lines) != Some(1) {
            return Err(Error::new(format!(
                "{arch}: the negative control should fail with `{NEGATIVE_FAILURE}` and exit 1, \
                 and did not.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: the negative control failed on the /proc thread count, as it must");
    }
    Ok(())
}

/// The line the stall probe prints its measurement on.
const STALL_MEASURED: &str = "threads: stall-probe ";

/// Guest memory the stall probe needs at least, in MiB: its 320 MiB process,
/// the copies its forks' children cause while they live, and the rest.
const STALL_MEMORY_MIB: u32 = 1536;

/// `stall-probe` (`docs/ROBUSTNESS.md` R5): boot `threads-test`'s stall-probe
/// step as init and require that another process's 1 ms sleep overran by no
/// more than 100 ms while a 320 MiB, 40-thread process forked four times, and
/// by no more than 50 ms while it was then killed and reaped: both interim
/// (ledger 8299 C, 8324 CT.1), lowered to 10 ms by R5's step 2. The program prints the figures and fails
/// past the budget; this requires its `ok` line and status 0, and repeats the
/// figures.
///
/// It fails on a kernel whose fork holds the address space's spin lock across
/// work in proportion to the resident pages, which is its control: a row run
/// on such a kernel FIRES with the program's own line.
pub(crate) fn stall_probe(args: &Args) -> Result<()> {
    let mut args = args.clone();
    args.memory = args.memory.max(STALL_MEMORY_MIB);
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");
        let program = build_flavour(arch, Flavour::StallProbe, false)?;
        let lines = boot(arch, &program, &args)?;
        let measured = lines.iter().find(|line| line.contains(STALL_MEASURED));
        if let Some(line) = measured {
            let at = line.find(STALL_MEASURED).unwrap_or(0);
            println!("  {arch}: {}", line.get(at..).unwrap_or(line).trim());
        }
        let passed = lines
            .iter()
            .any(|line| says(line, "threads: stall-probe ok"));
        if !passed || status(&lines) != Some(0) {
            let failed = lines.iter().find(|line| line.contains("threads: FAILED"));
            return Err(Error::new(format!(
                "{arch}: the stall probe did not pass{}.\n  Serial output is in {}",
                failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                log.display()
            )));
        }
        println!(
            "  {arch}: another process kept within the budgets across the forks and the teardown"
        );
    }
    Ok(())
}
