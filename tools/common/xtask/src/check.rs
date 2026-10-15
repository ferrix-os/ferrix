//! `cargo xtask check` — every gate CI runs, run locally in one command.
//!
//! The point is that a contributor never learns from CI something they could
//! have learned in a minute. The order is cheapest-first, so the gate most
//! likely to fail on a work-in-progress tree fails first.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::cargo::{self, cargo as cargo_binary};
use crate::paths::{self, Arch};
use crate::step_cache;
use crate::workspace;
use crate::{Error, Result};

/// The commands [`command`] answers: `check`, and its steps run alone.
pub(crate) const COMMANDS: &[&str] = &[
    "check",
    "miri",
    "loom",
    "check-ferrousli",
    "host-clippy",
    "host-test",
    "host-doctest",
    "host-doc",
    "check-apps",
    "check-side",
    "check-unit",
    "check-audit",
    "check-docs",
    "check-ferrocene",
    "gate-rows",
    "components",
    "pin-components",
];

/// `check`, or one of its steps run alone, by the command's name.
pub(crate) fn command(name: &str, args: &Args) -> Result<()> {
    match name {
        "miri" => miri_all(args.jobs),
        "loom" => loom(&paths::workspace_root()),
        "check-ferrousli" => ferrousli(&paths::workspace_root()),
        "host-clippy" => host_clippy(),
        "host-test" => match launch_host_tests()? {
            Some(launched) => launched.finish(),
            None => Ok(()),
        },
        "host-doctest" => host_doctest(),
        "host-doc" => host_doc(),
        "check-apps" => apps(&mut Counts::default()),
        "check-side" => side_workspaces(&paths::workspace_root()),
        "check-unit" => check_unit(&paths::workspace_root()),
        "check-audit" => check_audit(),
        "check-docs" => docs(),
        "check-ferrocene" => ferrocene_rust(),
        "gate-rows" => crate::gate_rows::run(args),
        "components" | "pin-components" => crate::components::command(name),
        _ => run(args),
    }
}

/// `check-docs`: what of `check` reads documentation, for a change to
/// `docs/` and top-level Markdown alone -- the commit hooks, and the audits
/// and generated-document checks, which say whether a document went stale
/// against the model, the code or the catalog it is generated from. No
/// cargo step: nothing under `docs/` is compiled into anything
/// (`docs/TEST-TIME.md`, Phase 3, B1; `cargo xtask gate-rows` picks it).
fn docs() -> Result<()> {
    step("commit hooks", commit_hooks)?;
    steps_at_once(AUDITS)?;
    println!("\nchecked (docs only: no cargo step)");
    Ok(())
}

/// The terminal's font is rasterised from the TrueType faces committed beside
/// it, by a rasteriser in the term app rather than by whatever `FreeType` the
/// machine has: that is what makes "byte-identical" a demand this gate can
/// make of every checkout. The generator is the app's own
/// (`tools/gen-font.py`, found through its folder, since 2026-10-04).
fn terminal_font() -> Result<()> {
    let script = crate::apps::folder("term")?.join("tools/gen-font.py");
    python_with(&script.to_string_lossy(), &["--check"])
}

/// The first step of every check: whether the commit hooks are armed.
fn commit_hooks() -> Result<()> {
    python_with("tools/common/check/check-commit-authors.py", &["--hooks"])
}

/// The audits and generated-document checks: each reads the tree and writes
/// nothing another reads (a `--check` compares in memory), so `check` runs
/// them at once and prints their output in this order afterwards
/// (`docs/TEST-TIME.md`, C1). Each is a step's name and the scripts it runs,
/// one after another, each with its arguments.
const AUDITS: &[(&str, &[&[&str]])] = &[
    (
        "line endings",
        &[&["tools/common/check/check-line-endings.py"]],
    ),
    (
        "assembly allow-list",
        &[&["tools/common/check/check-asm-budget.py"]],
    ),
    // The seam §7 is built on: the kernel enumerates devices and drives
    // none. A convenient register access in the wrong file is how that
    // claim decays, and it decays silently, so it is asserted here rather
    // than reviewed for.
    (
        "device-access allow-list",
        &[&["tools/common/check/check-device-access.py"]],
    ),
    (
        "unsafe audit",
        &[&["tools/common/check/check-unsafe-audit.py"]],
    ),
    (
        "panic audit",
        &[&["tools/common/check/check-panic-audit.py"]],
    ),
    // The boundary four assurance ratings attach to. Every artifact in
    // docs/certification is scoped to
    // `tools/common/data/certification-item.json`, so a kernel file that
    // drifts into the trusted core -- or an unclassified new one that
    // nobody decided about -- silently changes what those ratings claim.
    // docs/certification/ITEM.md.
    (
        "certification item boundary",
        &[&["tools/common/check/check-item-boundary.py"]],
    ),
    // A coding standard with metrics, which EN 50716 requires and the
    // other gates did not supply: how complicated one function in the
    // certified item may be, how long, and whether it calls itself. A
    // ratchet over a recorded baseline, like the item boundary above it.
    (
        "complexity budget",
        &[&["tools/common/check/check-complexity.py"]],
    ),
    // Allocation failure is an error the certified item reports, never a
    // stop (finding F-23). An allocating standard-library call in the
    // item's product code is refused unless it goes through
    // `src/kernel/src/fallible.rs` or is argued at the site; a ratchet
    // over a recorded baseline, like the two above.
    // docs/certification/MEMORY-AND-TIMING.md.
    (
        "fallible allocation",
        &[&["tools/common/check/check-fallible-alloc.py"]],
    ),
    // The safety manual is an out-of-context argument an integrator
    // designs against, so a claim in it that quietly stopped being true
    // would be worse than no manual. Every claim names its evidence, and
    // this fails when a citation stops resolving.
    // docs/certification/SAFETY-MANUAL.md.
    (
        "safety requirements",
        &[&["tools/common/check/check-safety-requirements.py"]],
    ),
    // The item's requirements, each naming its parent and, at the low
    // level, the function it is about, against the `/// Verifies:` tags
    // on the checks. A check naming an id nobody defines, or a
    // requirement that loses its verifier, fails here rather than in an
    // assessment. docs/certification/TRACEABILITY.md, which it
    // regenerates.
    (
        "traceability",
        &[&["tools/common/check/check-traceability.py", "--check"]],
    ),
    // The uncovered statements, sorted into what is argued and what is a
    // gap. Regenerated from the residual the coverage run writes, so the
    // two cannot disagree. docs/certification/COVERAGE-RESIDUAL.md.
    (
        "coverage residual",
        &[&["tools/common/gen/gen-coverage-justification.py", "--check"]],
    ),
    // The item links no external crate on any architecture, which is
    // what lets IEC 62304's SOUP obligation be answered with "none"
    // rather than with an anomaly-list evaluation per dependency. That is
    // a property worth re-establishing rather than remembering.
    // docs/certification/SOUP.md.
    (
        "SOUP register",
        &[&["tools/common/gen/gen-soup.py", "--check"]],
    ),
    // The architecture document is generated from `docs/sysml/` and
    // committed. A model edited without regenerating leaves the two
    // disagreeing, and the document is exactly where nobody would
    // notice; this is the cheapest possible place to say so.
    (
        "architecture document",
        &[
            &["tools/common/gen/sysml/tests.py"],
            &["tools/common/gen/gen-arch-doc.py", "--check"],
        ],
    ),
    // The roadmap's SUMMARY.md, the overview's stage index and the
    // docs/ROADMAP.md stub are generated from docs/roadmap/ by
    // split-roadmap.py, and every link into the roadmap has to land on a
    // heading. A page added by hand to SUMMARY.md and not to the
    // generator's table is dropped by the next `index`, silently; this
    // says so before it lands.
    (
        "roadmap index and links",
        &[&["tools/common/gen/split-roadmap.py", "check"]],
    ),
    // The compositor's interface tables are generated from the protocol
    // XML vendored beside them. A table edited by hand is a compositor
    // that reads a client's message with the wrong signature, which is
    // the kind of bug that shows up as one misdrawn window an hour later.
    (
        "wayland protocol tables",
        &[&["tools/common/gen/gen-wayland-protocol.py", "--check"]],
    ),
    // The keymap the compositor hands every client, and the modifier
    // bits that go with it, are libxkbcommon's own output through a
    // committed probe. A keymap edited by hand is a keyboard that types
    // the wrong letters, and the client is the only thing that would
    // notice.
    (
        "xkb keymap and tables",
        &[&["tools/common/gen/gen-xkb-tables.py", "--check"]],
    ),
    // init's system call, errno and group tables are generated from Linux's
    // headers and systemd's listing, committed beside them: a number typed
    // by hand would be a filter that lets through what it was asked to stop.
    (
        "init's system call tables",
        &[&["tools/common/gen/gen-init-syscall-tables.py", "--check"]],
    ),
    // The panic screen's font is generated from the BDF committed beside
    // it, and a hand edit to either would otherwise drift silently.
    ("font", &[&["tools/common/gen/gen-font.py", "--check"]]),
    // The explanations a panic prints are rendered into a document, which
    // goes stale the moment an entry changes without it.
    (
        "panic catalog",
        &[&["tools/common/gen/gen-panic-catalog.py", "--check"]],
    ),
];

/// Run the gate set.
fn run(args: &Args) -> Result<()> {
    let root = paths::workspace_root();

    // First because it is the cheapest, and because it is the gate that says
    // whether the *other* local gates -- the commit hooks -- are running at
    // all. They are files until `core.hooksPath` points at them, and a clone
    // where nobody ran that line refuses nothing.
    step("commit hooks", commit_hooks)?;

    step("formatting", || {
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&root)
            .args(["fmt", "--all", "--", "--check"]);
        cargo::run(command, "cargo fmt --check")
    })?;

    // The audits read the tree and write nothing a build reads, and each side
    // workspace builds in a target directory of its own (`own_target_dir`),
    // so neither waits for the main workspace's cargo steps: both run beside
    // them, and what they said is printed whole once those have ended
    // (`docs/TEST-TIME.md`, B4). Every part runs to its end whichever fails,
    // and the first failure in the order the steps used to run in is the
    // error.
    let beside = Beside::start()?;
    let ((said, audits), main) = std::thread::scope(|scope| {
        let audits = scope.spawn(|| at_once(AUDITS));
        let main = main_workspace(&root, args);
        let audits = audits.join().unwrap_or_else(|_| {
            (
                String::new(),
                Err(Error::new("the audits' thread panicked")),
            )
        });
        (audits, main)
    });
    println!("\n== the audits, run beside the steps above");
    print!("{said}");
    let side = beside.finish();
    audits?;
    let fast = main?;
    side?;
    if fast {
        println!("\nchecked (--fast: cross-target clippy skipped)");
    } else {
        println!("\nchecked");
    }
    Ok(())
}

/// The main workspace's steps of `check`, one after another, then
/// ferrousli's, zinc's and Miri's when asked for, then the cross-target
/// clippies unless `--fast`; whether `--fast` skipped them.
fn main_workspace(root: &std::path::Path, args: &Args) -> Result<bool> {
    step("terminal font", terminal_font)?;
    step("btrfs allocates fallibly", item_crates_allocate_fallibly)?;
    step(
        "the item with Ferrocene's Rust (1.95.0, upstream)",
        ferrocene_rust,
    )?;

    step("crate layering", || {
        let mut command = Command::new("bash");
        let _ = command
            .current_dir(root)
            .arg("tools/common/check/check-crate-layering.sh");
        cargo::run(command, "tools/common/check/check-crate-layering.sh")
    })?;

    // The runtime and the native programs are freestanding like the kernel:
    // a `_start`, a panic handler, a linker script. The host can build none of
    // them, so they are linted per target below, and what of them can be
    // tested lives in `src/lib/proto/native`.
    step("clippy (host)", host_clippy)?;
    // The test binaries run side by side with each other and with the steps
    // below, which build while they run; their outputs and verdict come
    // after those (`crate::test_runs`, `docs/TEST-TIME.md`, P1).
    let mut launched = None;
    step("tests", || {
        launched = launch_host_tests()?;
        Ok(())
    })?;
    let rest = after_the_tests(root, args);
    let tests = match launched {
        Some(launched) => step("tests: the binaries, side by side", || launched.finish()),
        None => Ok(()),
    };
    tests?;
    rest
}

/// The main workspace's steps after the tests: whether `--fast` skipped the
/// cross-target clippies.
fn after_the_tests(root: &std::path::Path, args: &Args) -> Result<bool> {
    step("doc tests", host_doctest)?;
    step("documentation", host_doc)?;

    if args.ferrousli {
        ferrousli(root)?;
    }

    if args.zinc {
        zinc(root)?;
    }

    // Opt-in, because it is minutes rather than seconds and needs a nightly
    // toolchain with the miri component: the same crates as CI's Miri job, so
    // a UB report can be reproduced before pushing. `cargo xtask miri` is the
    // same step without everything above it.
    if args.miri {
        miri_all(args.jobs)?;
    }

    if args.fast {
        return Ok(true);
    }

    cross_target_clippy()?;
    Ok(false)
}

/// The workspaces of their own that `check` reaches -- loom's, the
/// compositor's, the Linux programs', adbd's, nvos's and every app's -- one
/// after another: `cargo xtask check-side`, which `check` starts beside its
/// main workspace's steps ([`Beside`]).
fn side_workspaces(root: &std::path::Path) -> Result<()> {
    // loom's models are of the kernel's orderings: the item's, never cached
    // (the certification consultant, ledger line 1447).
    loom(root)?;
    let mut counts = Counts::default();
    counts.add(step_cache::unit(
        "compositor",
        &own_target_dir("compositor"),
        || compositor(root),
    )?);
    for (name, relative) in LINUX_WORKSPACES {
        counts.add(step_cache::unit(name, &own_target_dir(name), || {
            linux_workspace(root, name, relative)
        })?);
    }
    step("auth: no sabotage in the environment", no_sabotage)?;
    counts.add(step_cache::unit("media", &own_target_dir("media"), || {
        media(root)
    })?);
    counts.add(step_cache::unit("adbd", &own_target_dir("adbd"), || {
        adbd(root)
    })?);
    counts.add(step_cache::unit("nvos", &own_target_dir("nvos"), || {
        nvos(root)
    })?);
    apps(&mut counts)?;
    if step_cache::store().is_some() {
        println!(
            "\n-- the step cache: {} units ran, {} cached passes",
            counts.ran, counts.cached
        );
    }
    Ok(())
}

/// How many units ran and how many were cached passes.
#[derive(Debug, Default)]
struct Counts {
    /// Units that ran.
    ran: usize,
    /// Units a record stood for.
    cached: usize,
}

impl Counts {
    fn add(&mut self, outcome: step_cache::Outcome) {
        match outcome {
            step_cache::Outcome::Ran => self.ran += 1,
            step_cache::Outcome::Cached => self.cached += 1,
        }
    }
}

/// A system bus address with nothing at it.
const NO_SYSTEM_BUS: &str = "unix:path=/nonexistent/ferrix-check-has-no-system-bus";

/// The Linux-ABI workspaces `check` reaches as units, by name and place.
const LINUX_WORKSPACES: &[(&str, &str)] = &[
    ("init", "src/user/system/linux/init"),
    ("auth", "src/user/system/linux/auth"),
    ("pkg", "src/user/system/linux/pkg"),
    ("installer", "src/user/system/linux/installer"),
];

/// `cargo xtask check-unit`: the one unit `FERRIX_CHECK_UNIT` names, as
/// [`step_cache`] runs it under `strace`.
fn check_unit(root: &std::path::Path) -> Result<()> {
    let name = std::env::var(step_cache::UNIT_VARIABLE)
        .map_err(|_| Error::new("check-unit: FERRIX_CHECK_UNIT names no unit"))?;
    if let Some(app) = name.strip_prefix("app:") {
        let apps = crate::apps::discover()?;
        let app = apps
            .iter()
            .find(|found| found.recipe.package.name == app)
            .ok_or_else(|| Error::new(format!("check-unit: no app {app}")))?;
        return app_steps(app);
    }
    match name.as_str() {
        "compositor" => compositor(root),
        "media" => media(root),
        "adbd" => adbd(root),
        "nvos" => nvos(root),
        _ => match LINUX_WORKSPACES.iter().find(|(unit, _)| *unit == name) {
            Some((unit, relative)) => linux_workspace(root, unit, relative),
            None => Err(Error::new(format!("check-unit: no unit {name}"))),
        },
    }
}

/// `cargo xtask check-side`, started by `check` as a process of its own,
/// with what it says going to a log that is printed whole when it ends, so
/// its output never interleaves with the main workspace's.
#[derive(Debug)]
struct Beside {
    /// The process.
    child: std::process::Child,
    /// Where its output goes.
    log: std::path::PathBuf,
    /// When it was started.
    began: Instant,
}

impl Beside {
    /// Start it.
    fn start() -> Result<Beside> {
        let directory = paths::target_dir();
        std::fs::create_dir_all(&directory)?;
        let log = directory.join("check-side.log");
        let file = std::fs::File::create(&log)
            .map_err(|error| Error::new(format!("creating {}: {error}", log.display())))?;
        let child = Command::new(std::env::current_exe()?)
            .arg("check-side")
            .current_dir(paths::workspace_root())
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .spawn()
            .map_err(|error| Error::new(format!("could not start check-side: {error}")))?;
        println!(
            "\n   the workspaces of their own run beside the steps below; \
             their output follows them ({})",
            log.display()
        );
        Ok(Beside {
            child,
            log,
            began: Instant::now(),
        })
    }

    /// Wait for it, print what it said, and say whether it passed.
    fn finish(mut self) -> Result<()> {
        let status = self.child.wait();
        println!("\n== the workspaces of their own, run beside the steps above");
        match std::fs::read(&self.log) {
            Ok(said) => print!("{}", String::from_utf8_lossy(&said)),
            Err(error) => println!("reading {}: {error}", self.log.display()),
        }
        took("the workspaces of their own, beside", self.began);
        let status =
            status.map_err(|error| Error::new(format!("waiting for check-side: {error}")))?;
        cargo::finished(
            status,
            "the workspaces of their own (cargo xtask check-side)",
        )
    }
}

/// The kernel and the item's library crates, type-checked for every kernel
/// target with the upstream Rust that Ferrocene's newest release is built
/// from, so that no language or library feature newer than Ferrocene's
/// enters the code to be certified (`docs/roadmap/microkernel-m0.md`, E;
/// `docs/MICROKERNEL.md` section 6.5). Not Ferrocene's own compiler: that
/// needs the customer's subscription. The script says which version, why,
/// and what it cannot see; `cargo xtask check-ferrocene` runs it alone.
fn ferrocene_rust() -> Result<()> {
    python_with("tools/common/check/check-ferrocene-rust.py", &[])
}

/// The item crates that must make every allocation fallibly, by package.
const FALLIBLE_CRATES: [&str; 2] = ["ferrix-btrfs", "ferrix-btrfs-write"];

/// The btrfs crates allocate nothing that cannot fail: the fallible-allocation
/// gate finds no unmarked allocating call in either. Its ratchet alone would
/// let a count stand above zero once recorded; this holds it at zero.
///
/// Verifies: `L.btrfs.22`
fn item_crates_allocate_fallibly() -> Result<()> {
    let interpreter = python_interpreter().ok_or_else(|| {
        Error::new("no working Python interpreter on PATH (tried python3, python)")
    })?;
    let output = Command::new(interpreter)
        .current_dir(paths::workspace_root())
        .arg("tools/common/check/check-fallible-alloc.py")
        .output()
        .map_err(|error| Error::new(format!("check-fallible-alloc.py: {error}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let counts = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("fallible-alloc: item crates, unmarked allocating calls: ")
        })
        .ok_or_else(|| Error::new("check-fallible-alloc.py reported no item-crate counts"))?;
    for name in FALLIBLE_CRATES {
        let count = counts
            .split(", ")
            .find_map(|entry| entry.strip_prefix(name)?.strip_prefix(' '))
            .ok_or_else(|| Error::new(format!("check-fallible-alloc.py did not count {name}")))?;
        if count != "0" {
            return Err(Error::new(format!(
                "{name} makes {count} allocation(s) that cannot fail; it must make none"
            )));
        }
    }
    println!(
        "{} each make 0 allocations that cannot fail",
        FALLIBLE_CRATES.join(" and ")
    );
    Ok(())
}

/// The freestanding halves and the Pixel 7's loader, linted for their own
/// targets: what `--fast` skips.
fn cross_target_clippy() -> Result<()> {
    // The freestanding halves, once per target. A lint pass for x86-64 cannot
    // see Arm code at all, so skipping these means two thirds of the kernel go
    // unlinted until CI. The kernel for every target is one cargo call, and
    // the kernel `--mitigations off` another in its own target directory, so
    // the two run at once (`docs/TEST-TIME.md`, C1).
    step(
        "clippy (kernel, every architecture, and again --mitigations off)",
        kernel_clippy_both_settings,
    )?;
    for arch in Arch::ALL {
        step(&format!("clippy (loader, {arch})"), || {
            clippy(&["-p", "ferrix-boot", "--target", arch.loader_target()])
        })?;
        step(
            &format!("clippy (native runtime and programs, {arch})"),
            || native_clippy(arch),
        )?;
    }
    // The Pixel 7's loader is a binary of its own for one target, outside
    // the per-architecture loop. Linted without a payload, which is how it
    // builds when FERRIX_PIXEL7_KERNEL is unset. It went unlinted until an
    // unsafe block with two operations in it was found by hand.
    step("clippy (Pixel 7 loader, aarch64)", || {
        clippy(&[
            "-p",
            "ferrix-boot-pixel7",
            "--target",
            Arch::AArch64.kernel_target(),
        ])
    })?;
    Ok(())
}

/// ferrousli's gates, behind `--ferrousli`.
///
/// ferrousli is a workspace of its own, so none of the steps above reach it,
/// and a change elsewhere could break it without a gate noticing. These are
/// the gates its landings have always run. The tests run twice because each C
/// program is built at `-O0` and `-O2` in both profiles, and the library
/// itself behaves differently optimised: a release build is where the
/// optimiser turns loops into calls to the library's own `memcpy`.
///
/// Off by default: building the library and its C programs twice is minutes,
/// and most landings cannot affect it. `docs/BACKLOG.md` says which landings
/// must pass it.
///
/// On Windows every cargo step runs in WSL, for the reason `crate::wsl`
/// gives: the tests start Linux executables. The generated-ABI check reads
/// headers in the tree and runs natively.
fn ferrousli(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/ferrousli");
    let in_ferrousli = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command.current_dir(&dir).args(arguments);
        command
    };
    if cfg!(windows) {
        step("ferrousli: WSL", || {
            crate::wsl::require_toolchain("ferrousli's tests run C programs built for Linux")
        })?;
    }

    step("ferrousli: generated ABI", || {
        python_with(
            "src/user/system/linux/ferrousli/tools/gen-abi.py",
            &["--check"],
        )
    })?;
    step("ferrousli: formatting", || {
        cargo::run(in_ferrousli(&["fmt", "--check"]), "cargo fmt (ferrousli)")
    })?;
    // `--workspace`, because ferrousli's root is a package as well as a
    // workspace, and cargo run there without it covers that package alone:
    // the loader in `ld/` went ungated until 2026-09-21 for that reason.
    step("ferrousli: clippy", || {
        cargo::run(
            in_ferrousli(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ]),
            "cargo clippy (ferrousli)",
        )
    })?;
    // The library on AArch64 and ARMv7-A, whose code the host's build never
    // compiles. Clippy needs only their rustup targets; the tests there need
    // a cross compiler and QEMU's user mode, which `src/user/system/linux/ferrousli/README.md` says
    // how to run.
    for target in ["aarch64-unknown-linux-gnu", "armv7-unknown-linux-gnueabihf"] {
        step(&format!("ferrousli: clippy ({target})"), || {
            cargo::run(
                in_ferrousli(&[
                    "clippy", "--lib", "--target", target, "--", "-D", "warnings",
                ]),
                "cargo clippy (ferrousli)",
            )
        })?;
    }
    // The loader, which `--all-targets` skips: its binary is built only with
    // the `loader` feature and for a musl target (`ld/Cargo.toml`), and it
    // went unlinted until 2026-09-23 for that reason.
    for target in [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "armv7-unknown-linux-musleabihf",
    ] {
        step(&format!("ferrousli: clippy (loader, {target})"), || {
            cargo::run(
                in_ferrousli(&[
                    "clippy",
                    "-p",
                    "ferrousli-ld",
                    "--bin",
                    "ld-ferrousli",
                    "--features",
                    "loader",
                    "--target",
                    target,
                    "--",
                    "-D",
                    "warnings",
                ]),
                "cargo clippy (ferrousli's loader)",
            )
        })?;
    }
    step("ferrousli: tests", || {
        cargo::run(
            in_ferrousli(&["test", "--workspace"]),
            "cargo test (ferrousli)",
        )
    })?;
    step("ferrousli: tests (release)", || {
        cargo::run(
            in_ferrousli(&["test", "--workspace", "--release"]),
            "cargo test --release (ferrousli)",
        )
    })
}

/// zinc's gates, behind `--zinc`.
///
/// zinc is a workspace of its own too, and `build` only compiles it into the
/// initramfs. These are the gates a zinc landing runs: formatting, clippy,
/// the unit tests, and two gates driven through a pseudo-terminal: the line
/// editor's completion, which is the only way to see what a Tab does, and
/// job control, which is the only way to see a process group at all.
///
/// Clippy and the tests build with the `next` feature, so `zinc-next`, the
/// port of zsh's runtime that lands in slices, meets the same gate as
/// `zinc`. Clippy allows `excessive_nesting`: the runtime zinc landed with
/// carries that warning in its parser and builtins, which that port replaces
/// rather than restructures. Every other warning fails the gate.
///
/// The tests start Linux executables and the pty needs a Linux kernel, so on
/// Windows those steps run in WSL, as ferrousli's do.
/// The `loom` models of `src/tests/loom`: the orderings the kernel's wait
/// and its way back to user mode rest on (`docs/OPAQUE-KERNEL.md` §9.8, 2c's
/// case 9, 2e's condition 5 and 2f's regroup word), each with a control that
/// must fail.
///
/// A workspace of its own, as `src/tests/fuzz` is, so that loom and what it
/// pulls in stay out of the kernel's dependency graph and `deny.toml`
/// (the customer's decision of 2026-10-03). `--locked`, so the versions run
/// are the lock file's. Seconds: the models are small and run under a
/// preemption bound of three.
///
/// Verifies: L.sched.51
pub(crate) fn loom(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/tests/loom");
    let native = |arguments: &[&str]| {
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir("loom"));
        command
    };
    step("loom: formatting", || {
        cargo::run(native(&["fmt", "--check"]), "cargo fmt (loom)")
    })?;
    step("loom: clippy", || {
        cargo::run(
            native(&[
                "clippy",
                "--locked",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ]),
            "cargo clippy (loom)",
        )
    })?;
    step("loom: models", || {
        cargo::run(
            native(&["test", "--locked", "--release"]),
            "cargo test (loom models)",
        )
    })
}

fn zinc(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/zinc");
    const TARGET: &str = "x86_64-unknown-linux-musl";
    let native = |arguments: &[&str]| {
        let mut command = Command::new(cargo_binary());
        let _ = command.current_dir(&dir).args(arguments);
        command
    };
    step("zinc: formatting", || {
        cargo::run(native(&["fmt", "--check"]), "cargo fmt (zinc)")
    })?;
    step("zinc: clippy", || {
        cargo::run(
            native(&[
                "clippy",
                "--target",
                TARGET,
                "--all-targets",
                "--features",
                "next",
                "--",
                "-D",
                "warnings",
                "-A",
                "clippy::excessive_nesting",
            ]),
            "cargo clippy (zinc)",
        )
    })?;
    if cfg!(windows) {
        step("zinc: WSL", || {
            crate::wsl::require_toolchain("zinc's tests drive a pseudoterminal")
        })?;
    }
    step("zinc: tests", || {
        let command = if cfg!(windows) {
            crate::wsl::cargo(&dir, &["test", "--features", "next", "--target", TARGET])
        } else {
            native(&["test", "--features", "next", "--target", TARGET])
        };
        cargo::run(command, "cargo test (zinc)")
    })?;
    step("zinc: completion on a pty", || {
        let script = "cargo build --release --target \"$1\" && \
                      python3 tests/pty_completion.py \"$CARGO_TARGET_DIR/$1/release/zinc\"";
        let command = if cfg!(windows) {
            crate::wsl::bash(&dir, script, &[TARGET])
        } else {
            let mut command = Command::new("bash");
            let _ = command
                .current_dir(&dir)
                .env("CARGO_TARGET_DIR", paths::target_dir().join("zinc-check"))
                .args(["-c", script, "bash", TARGET]);
            command
        };
        cargo::run(
            command,
            "src/user/system/linux/zinc/tests/pty_completion.py",
        )
    })?;
    step("zinc: oh-my-zsh's git prompt on a pty", || {
        let script = "cargo build --release --target \"$1\" && \
                      python3 tests/pty_prompt_git.py \"$CARGO_TARGET_DIR/$1/release/zinc\"";
        let command = if cfg!(windows) {
            crate::wsl::bash(&dir, script, &[TARGET])
        } else {
            let mut command = Command::new("bash");
            let _ = command
                .current_dir(&dir)
                .env("CARGO_TARGET_DIR", paths::target_dir().join("zinc-check"))
                .args(["-c", script, "bash", TARGET]);
            command
        };
        cargo::run(
            command,
            "src/user/system/linux/zinc/tests/pty_prompt_git.py",
        )
    })?;
    step("zinc: job control on a pty", || {
        let script = "cargo build --release --target \"$1\" && \
                      python3 tests/pty_jobs.py \"$CARGO_TARGET_DIR/$1/release/zinc\"";
        let command = if cfg!(windows) {
            crate::wsl::bash(&dir, script, &[TARGET])
        } else {
            let mut command = Command::new("bash");
            let _ = command
                .current_dir(&dir)
                .env("CARGO_TARGET_DIR", paths::target_dir().join("zinc-check"))
                .args(["-c", script, "bash", TARGET]);
            command
        };
        cargo::run(command, "src/user/system/linux/zinc/tests/pty_jobs.py")
    })
}

/// The compositor's gates.
///
/// `src/user/system/linux/compositor/` is a workspace of its own, like ferrousli, so the steps
/// above never reach it. They are on by default, because they are seconds
/// rather than minutes.
///
/// They also go through WSL on Windows now, which the comment here used to
/// say would happen "when a crate needs a Linux host". `src/user/system/linux/compositor/virgl` is
/// that crate: `device.rs` holds an `OwnedFd` for a render node and
/// `vtest.rs` speaks virglrenderer's protocol over a `UnixStream`, neither of
/// which `std` has on Windows, and `drm`, `render` and `hyprix` all build on
/// it. So the host pass stopped compiling on Windows the day the GPU work
/// landed, with rustc's "cannot find `unix` in `os`", and `cargo xtask check`
/// could not finish on that host at all.
///
/// Running it in the distribution rather than excluding the crates keeps the
/// two hosts checking the same code: an exclusion would have left the GPU
/// path linted on Linux and nowhere else, which is the half of the tree most
/// worth linting and the half a Windows developer is most likely to be
/// changing.
fn compositor(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/compositor");
    let in_compositor = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir("compositor"))
            // No system bus: the GPU tests' virgl_test_server asks it
            // something, and a host daemon answers from state no step-cache
            // record holds (the certification consultant, ledger 1508). The
            // tests pass without it; it is the same with the cache on or off.
            .env("DBUS_SYSTEM_BUS_ADDRESS", NO_SYSTEM_BUS);
        command
    };
    if cfg!(windows) {
        step("compositor: WSL", || {
            crate::wsl::require_toolchain("the compositor opens render nodes and Unix sockets")
        })?;
    }
    step("compositor: formatting", || {
        cargo::run(in_compositor(&["fmt", "--check"]), "cargo fmt (compositor)")
    })?;
    step("compositor: clippy", || {
        cargo::run(
            in_compositor(&["clippy", "--all-targets", "--", "-D", "warnings"]),
            "cargo clippy (compositor)",
        )
    })?;
    step("compositor: tests", || {
        cargo::run(in_compositor(&["test"]), "cargo test (compositor)")
    })
}

/// The gates of every app under `src/user/apps/`, found by their manifests
/// (`docs/APPS.md` §5): nothing here names one. Each is a workspace of its
/// own, so the host steps above never reach it.
fn apps(counts: &mut Counts) -> Result<()> {
    let mut apps = Vec::new();
    step("apps: manifests", || {
        apps = crate::apps::discover()?;
        println!("  {} apps", apps.len());
        Ok(())
    })?;
    for app in &apps {
        let name = &app.recipe.package.name;
        // A search of the whole tree, so never a unit: it reads the item.
        step(
            &format!("app {name}: nothing outside its folder names it"),
            || crate::apps::stays_in_its_folder(app),
        )?;
        counts.add(step_cache::unit(
            &format!("app:{name}"),
            &paths::target_dir().join("apps").join(name),
            || app_steps(app),
        )?);
    }
    Ok(())
}

/// One app's formatting, clippy and tests.
fn app_steps(app: &crate::apps::App) -> Result<()> {
    let name = &app.recipe.package.name;
    step(&format!("app {name}: formatting"), || {
        crate::apps::formatting(app)
    })?;
    step(&format!("app {name}: clippy and tests (host)"), || {
        crate::apps::host(app)
    })?;
    step(&format!("app {name}: clippy (targets)"), || {
        crate::apps::targets(app)
    })
}

/// Every image build inherits this environment, and a set
/// `FERRIX_AUTH_SABOTAGE` would build its authd with a refusal turned off
/// (`src/user/system/linux/auth/authd/src/sabotage.rs`).
fn no_sabotage() -> Result<()> {
    match std::env::var_os("FERRIX_AUTH_SABOTAGE") {
        Some(_) => Err(Error::new(
            "FERRIX_AUTH_SABOTAGE is set; unset it: only test-auth --sabotage may sabotage authd",
        )),
        None => Ok(()),
    }
}

/// The gates of a Linux-ABI workspace under `src/user/system/linux/`: init's
/// (`src/user/system/linux/init/`) and authentication's (`src/user/system/linux/auth/`). Each is a
/// workspace of its own, as the compositor is, so the host steps above
/// never reach it. On by default, since they are seconds. What they are
/// built on -- `src/lib/init/svc`, `src/lib/proto/auth-proto`,
/// `src/lib/crypto/argon2` -- the host steps do reach.
fn linux_workspace(root: &std::path::Path, name: &str, relative: &str) -> Result<()> {
    let dir = root.join(relative);
    let in_workspace = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir(name));
        command
    };
    if cfg!(windows) {
        step(&format!("{name}: WSL"), || {
            crate::wsl::require_toolchain("it is a Linux program, with Linux's system calls")
        })?;
    }
    step(&format!("{name}: formatting"), || {
        cargo::run(
            in_workspace(&["fmt", "--check"]),
            &format!("cargo fmt ({name})"),
        )
    })?;
    step(&format!("{name}: clippy"), || {
        cargo::run(
            in_workspace(&["clippy", "--all-targets", "--", "-D", "warnings"]),
            &format!("cargo clippy ({name})"),
        )
    })?;
    step(&format!("{name}: tests"), || {
        cargo::run(in_workspace(&["test"]), &format!("cargo test ({name})"))
    })
}

/// adbd's gates: `src/user/system/linux/adbd/` is a workspace of its own, as the media
/// programs are. Its protocol is `src/lib/proto/adb`, which the main workspace's
/// tests cover; what is left to check here is its formatting and clippy,
/// seconds. `test-adb` is the gate that runs it (`docs/ADB.md`).
fn adbd(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/adbd");
    let in_adbd = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir("adbd"));
        command
    };
    if cfg!(windows) {
        step("adbd: WSL", || {
            crate::wsl::require_toolchain("adbd is a Linux program, with Linux's system calls")
        })?;
    }
    step("adbd: formatting", || {
        cargo::run(in_adbd(&["fmt", "--check"]), "cargo fmt (adbd)")
    })?;
    step("adbd: clippy", || {
        cargo::run(
            in_adbd(&["clippy", "--all-targets", "--", "-D", "warnings"]),
            "cargo clippy (adbd)",
        )
    })
}

/// ferrix-nvos's gates: `src/user/system/linux/drivers/nvrm/os/nvos/` is a
/// workspace of its own, the OS layer nvrm links. Its host tests hold every
/// refusal of the core loader (`rmcore`) and the `rdcr4` answer (`cpu`),
/// which `test-nvrm-link` meets only in part on the machine (`docs/NVIDIA.md`
/// §4.1, "The core"); none needs NVIDIA's fetch. Seconds.
fn nvos(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/drivers/nvrm/os/nvos");
    let in_nvos = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir("nvos"));
        command
    };
    if cfg!(windows) {
        step("nvos: WSL", || {
            crate::wsl::require_toolchain("ferrix-nvos is built for the Linux ABI")
        })?;
    }
    step("nvos: formatting", || {
        cargo::run(in_nvos(&["fmt", "--check"]), "cargo fmt (nvos)")
    })?;
    step("nvos: clippy", || {
        cargo::run(
            in_nvos(&["clippy", "--all-targets", "--", "-D", "warnings"]),
            "cargo clippy (nvos)",
        )
    })?;
    step("nvos: tests", || {
        cargo::run(in_nvos(&["test"]), "cargo test (nvos)")
    })
}

/// The media workspace's gates: `src/user/system/linux/media/` is a workspace of its own,
/// as the init is. On by default, since they are seconds: the resampler is
/// tested on the host.
fn media(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/user/system/linux/media");
    let in_media = |arguments: &[&str]| {
        if cfg!(windows) {
            return crate::wsl::cargo(&dir, arguments);
        }
        let mut command = Command::new(cargo_binary());
        let _ = command
            .current_dir(&dir)
            .args(arguments)
            .env("CARGO_TARGET_DIR", own_target_dir("media"));
        command
    };
    if cfg!(windows) {
        step("media: WSL", || {
            crate::wsl::require_toolchain(
                "the sound server is a Linux program, with Linux's system calls",
            )
        })?;
    }
    step("media: formatting", || {
        cargo::run(in_media(&["fmt", "--check"]), "cargo fmt (media)")
    })?;
    step("media: clippy", || {
        cargo::run(
            in_media(&["clippy", "--all-targets", "--", "-D", "warnings"]),
            "cargo clippy (media)",
        )
    })?;
    step("media: tests", || {
        cargo::run(in_media(&["test"]), "cargo test (media)")
    })
}

/// `cargo xtask model-doc` -- regenerate the document the gate above checks.
pub(crate) fn model_doc() -> Result<()> {
    python_with("tools/common/gen/gen-arch-doc.py", &[])
}

// The host half of the gate, one function per CI step. `cargo xtask check`
// runs them and CI calls them by name (`cargo xtask host-test` and the rest),
// so the two cannot disagree about which members the host builds: both ask
// `workspace::members`.

/// The workspace's members, sorted.
/// The target directory `check` builds the workspace `name` in -- one
/// beside the main workspace's, never the main workspace's own.
///
/// Each workspace has its own lock file and profiles, and builds the same
/// crates from crates.io (`proc-macro2`, `syn`, `thiserror`) at the same
/// versions. In one shared directory cargo gives such a crate the same file
/// names in every workspace but a different fingerprint, so each workspace's
/// build threw away the one before it: in a warm `check` in a gate slot,
/// whose `CARGO_TARGET_DIR` every workspace inherited, the main clippy
/// compiled the proc macros again after the media workspace, the tests
/// compiled what uses them again after the clippy, and the compositor's and
/// media's steps did the same (`docs/TEST-TIME.md`, B3). A directory each
/// keeps each one's builds.
fn own_target_dir(name: &str) -> std::path::PathBuf {
    paths::target_dir().join(format!("check-{name}"))
}

fn sorted_members() -> Result<workspace::Members> {
    workspace::members(&paths::workspace_root())
}

/// `cargo clippy` over every host member, every target: `cargo xtask host-clippy`.
pub(crate) fn host_clippy() -> Result<()> {
    let excludes = sorted_members()?.excludes();
    let mut arguments: Vec<&str> = vec!["--workspace"];
    arguments.extend(excludes.iter().map(String::as_str));
    arguments.push("--all-targets");
    clippy(&arguments)
}

/// `cargo test` over every host member, every target: `cargo xtask host-test`.
pub(crate) fn host_test() -> Result<()> {
    host_cargo(&["test"], &["--all-targets"], &[], "cargo test")
}

/// [`host_test`] with the test binaries started through this program as
/// cargo's runner and left running (`crate::test_runs`), or `None` when
/// they ran in turn, as `FERRIX_TESTS_IN_TURN` or a non-Unix host asks.
fn launch_host_tests() -> Result<Option<crate::test_runs::Launched>> {
    if !crate::test_runs::side_by_side() {
        host_test()?;
        return Ok(None);
    }
    let directory = paths::target_dir()
        .join("test-runs")
        .join(std::process::id().to_string());
    let mut arguments: Vec<String> = vec!["test".to_owned(), "--workspace".to_owned()];
    arguments.extend(sorted_members()?.excludes());
    arguments.push("--all-targets".to_owned());
    let cache = crate::test_cache::plan(&paths::workspace_root())?;
    crate::test_runs::launch(
        &cargo_binary(),
        &paths::workspace_root(),
        &arguments,
        &directory,
        cache.as_deref(),
    )
    .map(Some)
}

/// The doc tests `--all-targets` skips: `cargo xtask host-doctest`.
pub(crate) fn host_doctest() -> Result<()> {
    host_cargo(&["test"], &["--doc"], &[], "cargo test --doc")
}

/// The documentation, warnings denied, as the lint config denies broken
/// intra-doc links: `cargo xtask host-doc`.
pub(crate) fn host_doc() -> Result<()> {
    documentation_in_memory(&paths::target_dir())?;
    host_cargo(
        &["doc"],
        &["--no-deps"],
        &[("RUSTDOCFLAGS", "-D warnings")],
        "cargo doc",
    )
}

/// Make `<target>/doc`, where `cargo doc` writes, a link to a directory in
/// memory: `$XDG_RUNTIME_DIR/ferrix-doc/<the SHA-256 of the target
/// directory's path>`.
///
/// The documentation step wants rustdoc's verdict, not its pages, which
/// nothing reads. Each of the ~105 rustdocs `cargo doc` runs at once takes
/// a lock on the output directory and, holding it, rewrites the files the
/// crates share (the search index, the implementations of each trait); on
/// nazuna's ext4 a file rewritten so is pushed toward the disk when it is
/// closed, and with the disk busy each crate held the lock for a second or
/// more, so the crates were documented one after another: 70-170 s of a
/// step whose rustdocs spend 21-25 CPU-s, 2,413 s of it in `flock` across
/// the rustdocs (`docs/TEST-TIME.md`, *Cut D1*). In memory it is 9-23 s.
///
/// The pages (about 160 MB) stay there until the runtime directory is
/// emptied (a log out, a reboot); cargo then finds them missing and
/// documents every crate again, once. The pages of a target directory that
/// is gone, or no longer links to them, are removed here. With no runtime
/// directory, under [`MEMORY_KEPT_FREE`] free in it, or off Unix, the
/// documentation is written to `<target>/doc` as cargo always did.
///
/// # Errors
///
/// When the link or the directory cannot be made or removed.
fn documentation_in_memory(target: &std::path::Path) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    match runtime {
        Some(runtime)
            if cfg!(unix)
                && runtime.is_dir()
                && free_bytes(&runtime).is_some_and(|free| free >= MEMORY_KEPT_FREE) =>
        {
            prune_pages(&runtime.join("ferrix-doc"))?;
            doc_link(target, &runtime)
        }
        _ => doc_on_disk(target),
    }
}

/// What the runtime directory must have free for the pages to go there: a
/// full one would fail the step, and the session's other users with it.
const MEMORY_KEPT_FREE: u64 = 1 << 30;

/// The bytes free in the file system holding `directory`, as `df` says;
/// `None` when it cannot say.
fn free_bytes(directory: &std::path::Path) -> Option<u64> {
    let output = Command::new("df")
        .args(["--output=avail", "-B1"])
        .arg(directory)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .nth(1)?
        .trim()
        .parse()
        .ok()
}

/// `<target>/doc` a directory of its own again, if it was a link.
fn doc_on_disk(target: &std::path::Path) -> Result<()> {
    let link = target.join("doc");
    if std::fs::symlink_metadata(&link).is_ok_and(|found| found.file_type().is_symlink()) {
        std::fs::remove_file(&link)?;
    }
    Ok(())
}

/// [`documentation_in_memory`] with the runtime directory given:
/// `<target>/doc` made a link to `<runtime>/ferrix-doc/<digest>`, whatever
/// was there before (a directory of pages, a link elsewhere) removed first,
/// and `<digest>.target` beside the pages naming the target directory, for
/// [`prune_pages`]. The pages' directory is made last, so pruning never
/// removes a directory a link already names.
fn doc_link(target: &std::path::Path, runtime: &std::path::Path) -> Result<()> {
    let target = std::path::absolute(target)?;
    let digest = crate::sha256::hex(target.as_os_str().as_encoded_bytes());
    let store = runtime.join("ferrix-doc");
    let pages = store.join(&digest);
    std::fs::create_dir_all(&store)?;
    std::fs::write(
        store.join(format!("{digest}.target")),
        target.as_os_str().as_encoded_bytes(),
    )?;
    std::fs::create_dir_all(&target)?;
    let link = target.join("doc");
    let linked = match std::fs::symlink_metadata(&link) {
        Ok(found) if found.file_type().is_symlink() => {
            let here = std::fs::read_link(&link)? == pages;
            if !here {
                std::fs::remove_file(&link)?;
            }
            here
        }
        Ok(found) if found.is_dir() => {
            std::fs::remove_dir_all(&link)?;
            false
        }
        Ok(_) => {
            std::fs::remove_file(&link)?;
            false
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    #[cfg(unix)]
    if !linked {
        std::os::unix::fs::symlink(&pages, &link)?;
    }
    std::fs::create_dir_all(&pages)?;
    Ok(())
}

/// Remove from `store` the pages of every target directory whose `doc` no
/// longer links to them (the directory deleted, or its pages moved), with
/// the file naming it. Pages with no such file are left alone.
fn prune_pages(store: &std::path::Path) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(store) else {
        return Ok(());
    };
    for entry in entries {
        let marker = entry?.path();
        let Some(digest) = marker
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".target"))
        else {
            continue;
        };
        let pages = store.join(digest);
        let Ok(named) = std::fs::read(&marker) else {
            continue;
        };
        let target = std::path::PathBuf::from(String::from_utf8_lossy(&named).into_owned());
        if std::fs::read_link(target.join("doc")).is_ok_and(|linked| linked == pages) {
            continue;
        }
        if pages.is_dir() {
            std::fs::remove_dir_all(&pages)?;
        }
        std::fs::remove_file(&marker)?;
    }
    Ok(())
}

/// `cargo clippy` over the native runtime and programs for `arch`'s kernel
/// target: `cargo xtask native-clippy --arch ARCH`.
pub(crate) fn native_clippy(arch: Arch) -> Result<()> {
    let native = sorted_members()?.native;
    let mut arguments: Vec<&str> = native
        .iter()
        .flat_map(|package| ["-p", package.as_str()])
        .collect();
    arguments.extend(["--target", arch.kernel_target()]);
    clippy(&arguments)
}

/// `cargo <command> --workspace` without the freestanding members, then
/// `extra`, with `environment` set.
fn host_cargo(
    command: &[&str],
    extra: &[&str],
    environment: &[(&str, &str)],
    what: &str,
) -> Result<()> {
    let excludes = sorted_members()?.excludes();
    let mut process = Command::new(cargo_binary());
    let _ = process
        .current_dir(paths::workspace_root())
        .args(command)
        .arg("--workspace")
        .args(&excludes)
        .args(extra)
        .envs(environment.iter().copied());
    cargo::run(process, what)
}

/// The crates CI's Miri jobs interpret, in their order.
///
/// A test below reads `.github/workflows/ci.yml` and fails when the two
/// disagree, so a step added to one and not the other is found by `cargo
/// xtask check` rather than by a contributor who trusted `--miri`.
const MIRI_PACKAGES: [&str; 17] = [
    "ferrix-elf",
    "ferrix-bootinfo",
    "ferrix-ustack",
    "ferrix-objects",
    "ferrix-vfs",
    "ferrix-pci",
    "ferrix-block",
    "ferrix-blkring",
    "ferrix-native",
    "ferrix-virtio-blk",
    "ferrix-frame",
    "ferrix-heap",
    "ferrix-fallible",
    "ferrix-paging",
    "ferrix-svc",
    "ferrix-argon2",
    "ferrix-seccomp",
];

/// CI's Miri steps: every crate in [`MIRI_PACKAGES`], `jobs` at a time.
///
/// `cargo xtask miri` runs this alone, and `check --miri` last. Miri builds
/// nothing but each crate's MIR, which takes seconds; the time is all
/// interpretation, one thread per crate, so the crates run side by side. One
/// interpreter held at most 400 MiB on 2026-09-28, so eight at once fit
/// beside everything else a shared host is doing. Each crate's output is kept
/// and printed whole when it ends, so the logs of crates run together do not
/// interleave, and every crate runs whichever fail.
pub(crate) fn miri_all(jobs: Option<u32>) -> Result<()> {
    step("miri setup", || {
        cargo::run(miri_command(&["setup"]), "cargo +nightly miri setup")
    })?;
    let jobs = jobs.map_or_else(
        || {
            std::thread::available_parallelism()
                .map_or(1, std::num::NonZero::get)
                .min(8)
        },
        |jobs| usize::try_from(jobs).unwrap_or(usize::MAX),
    );
    println!(
        "\n== miri: {} crates, {} at a time",
        MIRI_PACKAGES.len(),
        jobs.min(MIRI_PACKAGES.len())
    );
    let queue = std::sync::Mutex::new(MIRI_PACKAGES.iter());
    let failed = std::sync::Mutex::new(Vec::new());
    let started = Instant::now();
    let worker = || {
        loop {
            // Its own statement, so the queue's guard is dropped here: in a
            // `while let` it would live through the crate's whole run, and
            // the crates would take turns.
            let next = lock(&queue).next();
            let Some(package) = next else { break };
            if !miri_one(package) {
                lock(&failed).push(*package);
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(MIRI_PACKAGES.len()) {
            let _ = scope.spawn(worker);
        }
    });
    let failed = failed
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let elapsed = started.elapsed().as_secs();
    if failed.is_empty() {
        println!(
            "\n== miri: every crate passed in {}m{:02}s",
            elapsed / 60,
            elapsed % 60
        );
        Ok(())
    } else {
        Err(Error::new(format!("miri failed in {}", failed.join(", "))))
    }
}

/// `cargo +nightly miri test -p <package> --lib`, its output printed in one
/// piece when it ends; whether it passed.
fn miri_one(package: &str) -> bool {
    let started = Instant::now();
    let output = miri_command(&["test", "-p", package, "--lib"])
        .stdin(Stdio::null())
        .output();
    let elapsed = started.elapsed().as_secs();
    let took = format!("{}m{:02}s", elapsed / 60, elapsed % 60);
    match output {
        Ok(output) if output.status.success() => {
            println!("miri ({package}): passed in {took}");
            true
        }
        Ok(output) => {
            println!(
                "\n== miri ({package}): FAILED after {took}\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            false
        }
        Err(error) => {
            println!("miri ({package}): could not run cargo: {error}");
            false
        }
    }
}

/// `cargo +nightly miri <arguments>` in the workspace.
///
/// Through the rustup proxy by name rather than [`cargo_binary`]: `CARGO` is
/// the pinned toolchain's own cargo, which does not understand `+nightly`. The
/// variables the outer cargo exported would otherwise pin the inner one back
/// to that toolchain.
fn miri_command(arguments: &[&str]) -> Command {
    let mut command = Command::new("cargo");
    let _ = command
        .current_dir(paths::workspace_root())
        .env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env_remove("RUSTDOC")
        .args(["+nightly", "miri"])
        .args(arguments);
    command
}

/// A mutex's value, whether or not a thread panicked holding it: the queue
/// and the failure list stay whole either way.
fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Announce a gate, run it, and report.
fn step(name: &str, body: impl FnOnce() -> Result<()>) -> Result<()> {
    println!("\n== {name}");
    let started = Instant::now();
    let result = body();
    took(name, started);
    result
}

/// The line that says how long a step took, so that where `check`'s minutes
/// go can be read off its log (`docs/TEST-TIME.md`, C1).
fn took(name: &str, started: Instant) {
    println!("-- {name}: {:.1} s", started.elapsed().as_secs_f64());
}

/// Steps that read the tree and write nothing it reads, run at once: each
/// one's output is kept and printed whole, in the order given, once all have
/// ended, so a log reads as if they had run one after another. Every one
/// runs even after one fails, and the first failure in that order is the
/// error.
fn steps_at_once(steps: &[(&str, &[&[&str]])]) -> Result<()> {
    let (said, result) = at_once(steps);
    print!("{said}");
    result
}

/// [`steps_at_once`], with what it would print returned rather than
/// printed, for a caller that runs other steps meanwhile.
fn at_once(steps: &[(&str, &[&[&str]])]) -> (String, Result<()>) {
    use std::fmt::Write as _;
    let started = Instant::now();
    let results: Vec<(Duration, Result<Vec<u8>>)> = std::thread::scope(|scope| {
        let running: Vec<_> = steps
            .iter()
            .map(|(name, scripts)| scope.spawn(move || audit(name, scripts)))
            .collect();
        running
            .into_iter()
            .map(|handle| {
                handle.join().unwrap_or_else(|_| {
                    (
                        Duration::ZERO,
                        Err(Error::new("a check step's thread panicked")),
                    )
                })
            })
            .collect()
    });
    let mut first_failure = None;
    let mut text = String::new();
    for ((name, _), (spent, result)) in steps.iter().zip(results) {
        let _ = writeln!(text, "\n== {name}");
        match result {
            Ok(said) => text.push_str(&String::from_utf8_lossy(&said)),
            Err(error) => {
                let _ = writeln!(text, "{error}");
                let _ = first_failure.get_or_insert(error);
            }
        }
        let _ = writeln!(text, "-- {name}: {:.1} s", spent.as_secs_f64());
    }
    let _ = writeln!(
        text,
        "-- the steps above, at once: {:.1} s",
        started.elapsed().as_secs_f64()
    );
    (text, first_failure.map_or(Ok(()), Err))
}

/// One of [`steps_at_once`]'s steps: its scripts one after another, stopping
/// at the first that fails, with how long they took and what they printed.
fn scripts_in_turn(scripts: &[&[&str]]) -> (Duration, Result<Vec<u8>>) {
    let began = Instant::now();
    let mut said = Vec::new();
    for script in scripts {
        let Some((name, arguments)) = script.split_first() else {
            continue;
        };
        if let Err(error) = python_captured(name, arguments, &mut said) {
            return (began.elapsed(), Err(error));
        }
    }
    (began.elapsed(), Ok(said))
}

/// Steps of [`AUDITS`] the step cache leaves alone, each with why: they
/// run as with the cache off.
const AUDITS_UNCACHED: &[(&str, &str)] = &[
    (
        "line endings",
        "it looks at every file of the tree, among them what the test binaries running beside it write under build/ (a miss on every run), and tracing makes its 1.6 s 30",
    ),
    (
        "SOUP register",
        "it runs cargo, which reads the target directory's outputs (never recorded)",
    ),
];

/// One step of [`AUDITS`]: through the step cache when it is on, as a unit
/// keyed on the whole item (`step_cache::item_keyed`; `docs/TEST-TIME.md`,
/// Cut B6), otherwise its scripts in turn. What it said, and whether it
/// passed.
fn audit(name: &str, scripts: &[&[&str]]) -> (Duration, Result<Vec<u8>>) {
    let began = Instant::now();
    let store =
        step_cache::store().filter(|_| cfg!(target_os = "linux") && step_cache::strace_works());
    let Some(store) = store else {
        return scripts_in_turn(scripts);
    };
    let unit = format!("audit:{name}");
    if let Some((_, why)) = AUDITS_UNCACHED.iter().find(|(step, _)| *step == name) {
        let (spent, result) = scripts_in_turn(scripts);
        let note = format!("   {unit}: ran (never cached: {why})\n");
        return (spent, result.map(|said| [said, note.into_bytes()].concat()));
    }
    let context = match step_cache::item_keyed() {
        Ok(context) => context,
        Err(error) => {
            let (spent, result) = scripts_in_turn(scripts);
            let note = format!("   {unit}: ran, not recorded: {error}\n");
            return (spent, result.map(|said| [said, note.into_bytes()].concat()));
        }
    };
    let directory = step_cache::unit_directory(&store, &unit);
    if let Some((origin, inputs)) = step_cache::cached(&unit, &directory, &context) {
        let said = format!("   {unit}: cached pass {origin} ({inputs} inputs unchanged)\n");
        return (began.elapsed(), Ok(said.into_bytes()));
    }
    let result = audit_traced(&unit, &directory, &context);
    (began.elapsed(), result)
}

/// Run audit `unit` as `cargo xtask check-audit` under `strace`, and record
/// it when it passes and its trace allows: what it said.
fn audit_traced(
    unit: &str,
    directory: &std::path::Path,
    context: &step_cache::Context,
) -> Result<Vec<u8>> {
    std::fs::create_dir_all(directory)?;
    let traces = directory.join(format!("trace-{}", std::process::id()));
    let output = step_cache::strace(&traces, true)?
        .arg(std::env::current_exe()?)
        .arg("check-audit")
        .current_dir(context.root())
        .env(step_cache::UNIT_VARIABLE, unit)
        // Python writes no byte-code beside the scripts: a write in the
        // tree is never recorded.
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::new(format!("could not start strace for {unit}: {error}")))?;
    let mut said = output.stdout;
    said.extend_from_slice(&output.stderr);
    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&traces);
        return Err(Error::new(format!(
            "{}{unit} failed ({}; it ran as `cargo xtask check-audit` under strace)",
            String::from_utf8_lossy(&said),
            output.status
        )));
    }
    let note = step_cache::record_pass(unit, &traces, context.root(), directory, context)?;
    said.extend_from_slice(format!("   {unit}: {note}\n").as_bytes());
    Ok(said)
}

/// `cargo xtask check-audit`: the step of [`AUDITS`] `FERRIX_CHECK_UNIT`
/// names (`audit:<name>`), its scripts in turn, as `check` runs it under
/// `strace`.
fn check_audit() -> Result<()> {
    let unit = std::env::var(step_cache::UNIT_VARIABLE)
        .map_err(|_| Error::new("check-audit: FERRIX_CHECK_UNIT names no audit"))?;
    let name = unit.strip_prefix("audit:").unwrap_or_default();
    let (_, scripts) = AUDITS
        .iter()
        .find(|(step, _)| *step == name)
        .ok_or_else(|| Error::new(format!("check-audit: no audit {unit}")))?;
    for script in *scripts {
        if let Some((script, arguments)) = script.split_first() {
            python_with(script, arguments)?;
        }
    }
    Ok(())
}

/// [`python_with`], with what the script prints added to `said` rather than
/// shown, and its failure carrying that output.
fn python_captured(script: &str, arguments: &[&str], said: &mut Vec<u8>) -> Result<()> {
    let interpreter = python_interpreter().ok_or_else(|| {
        Error::new("no working Python interpreter on PATH (tried python3, python)")
    })?;
    let output = Command::new(interpreter)
        .current_dir(paths::workspace_root())
        .arg(script)
        .args(arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::new(format!("could not run {script}: {error}")))?;
    said.extend_from_slice(&output.stdout);
    said.extend_from_slice(&output.stderr);
    if output.status.success() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "{}{script} failed ({})",
            String::from_utf8_lossy(said),
            output.status
        )))
    }
}

/// `cargo clippy` over the kernel for every architecture, and at the same
/// time over the kernel built `--mitigations off`.
///
/// That is the kernel's one build setting, the other way: it compiles out
/// every side-channel defence, and code only one setting builds rots in the
/// other. It is linted in the target directory `cargo::kernel` builds that
/// setting into, so neither setting's cache is thrown away for the other's,
/// and cargo's lock on one directory never waits for the other.
///
/// Every one of the kernel's packages is linted: the binary and whatever
/// library crates sit beneath `src/kernel` (`docs/KERNEL-CRATES.md`), as
/// `workspace::members` finds them, so a crate split out of the kernel
/// cannot leave the lint pass by being missed from a list.
fn kernel_clippy_both_settings() -> Result<()> {
    let packages = workspace::members(&paths::workspace_root())?.kernel;
    if !packages.iter().any(|name| name == "ferrix-kernel") {
        return Err(Error::new(
            "the workspace has no ferrix-kernel package under src/kernel to lint",
        ));
    }
    let mut with_defences: Vec<&str> = Vec::new();
    for package in &packages {
        with_defences.extend(["-p", package.as_str()]);
    }
    for arch in Arch::ALL {
        with_defences.extend(["--target", arch.kernel_target()]);
    }
    let configs: Vec<String> = Arch::ALL
        .iter()
        .map(|arch| cargo::mitigations_off_config(arch.kernel_target()))
        .collect();
    let directory = cargo::mitigations_off_target_dir();
    let directory = directory.to_string_lossy();
    let mut without = with_defences.clone();
    for config in &configs {
        without.extend(["--config", config]);
    }
    without.extend(["--target-dir", &directory]);
    // The second is captured and printed after the first, so the two passes'
    // diagnostics never interleave.
    let (first, second) = std::thread::scope(|scope| {
        let off = scope.spawn(|| clippy_captured(&without));
        let on = clippy(&with_defences);
        let off = off
            .join()
            .unwrap_or_else(|_| Err(Error::new("the --mitigations off clippy thread panicked")));
        (on, off)
    });
    println!("\n   -- the same, --mitigations off:");
    second?;
    first
}

/// [`clippy`], with its output kept and printed whole when it ends.
fn clippy_captured(arguments: &[&str]) -> Result<()> {
    let output = Command::new(cargo_binary())
        .current_dir(paths::workspace_root())
        .arg("clippy")
        .args(arguments)
        .args(["--", "-D", "warnings"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::new(format!("could not run cargo clippy: {error}")))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    cargo::finished(output.status, "cargo clippy --mitigations off")
}

/// `cargo clippy ... -- -D warnings`.
fn clippy(arguments: &[&str]) -> Result<()> {
    let mut command = Command::new(cargo_binary());
    let _ = command
        .current_dir(paths::workspace_root())
        .arg("clippy")
        .args(arguments)
        .args(["--", "-D", "warnings"]);
    cargo::run(command, "cargo clippy")
}

/// Run one of the gate scripts, passing it arguments.
///
/// The interpreter is *probed*, not guessed. `python3` exists on a stock
/// Windows install as an App Execution Alias that is not Python at all: it
/// prints an advertisement for the Microsoft Store and exits 9009. Looking the
/// name up on PATH finds it, so the only reliable test is to run it.
pub(crate) fn python_with(script: &str, arguments: &[&str]) -> Result<()> {
    let interpreter = python_interpreter().ok_or_else(|| {
        Error::new("no working Python interpreter on PATH (tried python3, python)")
    })?;

    let mut command = Command::new(interpreter);
    let _ = command
        .current_dir(paths::workspace_root())
        .arg(script)
        .args(arguments);
    cargo::run(command, script)
}

/// The first name on PATH that answers `--version` like an interpreter.
pub(crate) fn python_interpreter() -> Option<&'static str> {
    ["python3", "python", "py"].into_iter().find(|name| {
        Command::new(name)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory of its own under the target directory.
    fn scratch(name: &str) -> std::path::PathBuf {
        let path = paths::target_dir()
            .join("xtask-check-test")
            .join(format!("{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn the_documentation_is_written_to_the_runtime_directory_whatever_was_there() {
        let root = scratch("doc-link");
        let runtime = root.join("runtime");
        std::fs::create_dir_all(&runtime).unwrap();
        let target = root.join("target");
        let link = target.join("doc");

        // Pages cargo wrote before: replaced by the link.
        std::fs::create_dir_all(link.join("ferrix_elf")).unwrap();
        std::fs::write(link.join("ferrix_elf/index.html"), "old").unwrap();
        doc_link(&target, &runtime).unwrap();
        let pages = std::fs::read_link(&link).unwrap();
        assert!(
            pages.starts_with(runtime.join("ferrix-doc")),
            "{}",
            pages.display()
        );
        assert!(pages.is_dir());

        // Run again: the same link, and what was written through it kept.
        std::fs::write(link.join("kept.html"), "new").unwrap();
        doc_link(&target, &runtime).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), pages);
        assert_eq!(
            std::fs::read_to_string(pages.join("kept.html")).unwrap(),
            "new"
        );

        // A link elsewhere, and the runtime directory emptied: made again.
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere"), &link).unwrap();
        std::fs::remove_dir_all(runtime.join("ferrix-doc")).unwrap();
        doc_link(&target, &runtime).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), pages);
        assert!(pages.is_dir());

        // Another target directory has pages of its own.
        let other = root.join("other-target");
        doc_link(&other, &runtime).unwrap();
        let others = std::fs::read_link(other.join("doc")).unwrap();
        assert_ne!(others, pages);

        // Pruning keeps both while each links to its pages; the other's
        // directory deleted, its pages and the file naming it go.
        let store = runtime.join("ferrix-doc");
        prune_pages(&store).unwrap();
        assert!(pages.is_dir() && others.is_dir());
        std::fs::remove_dir_all(&other).unwrap();
        prune_pages(&store).unwrap();
        assert!(pages.is_dir());
        assert!(!others.exists());
        let mut left: Vec<_> = std::fs::read_dir(&store)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        left.sort();
        assert_eq!(left.len(), 2, "{left:?}");

        // Pages on disk again: the link goes, and cargo makes a directory.
        doc_on_disk(&target).unwrap();
        assert!(std::fs::symlink_metadata(&link).is_err());
        doc_on_disk(&target).unwrap();
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn free_space_is_read_from_df() {
        let free = free_bytes(&paths::target_dir());
        assert!(free.is_some_and(|bytes| bytes > 0), "{free:?}");
        assert_eq!(free_bytes(std::path::Path::new("/no/such/directory")), None);
    }

    #[test]
    fn every_audit_left_uncached_is_an_audit() {
        for (name, _) in AUDITS_UNCACHED {
            assert!(AUDITS.iter().any(|(step, _)| step == name), "{name}");
        }
    }

    #[test]
    fn miri_runs_the_crates_ci_interprets_in_the_same_order() {
        let workflow =
            std::fs::read_to_string(paths::workspace_root().join(".github/workflows/ci.yml"))
                .unwrap();
        let in_ci: Vec<&str> = workflow
            .lines()
            .filter_map(|line| {
                let rest = line
                    .trim()
                    .strip_prefix("run: cargo +nightly miri test -p ")?;
                rest.split_whitespace().next()
            })
            .collect();
        assert_eq!(
            in_ci, MIRI_PACKAGES,
            "`cargo xtask check --miri` and CI's Miri job must name the same \
             crates; change MIRI_PACKAGES in tools/common/xtask/src/check.rs with the workflow"
        );
    }
}
