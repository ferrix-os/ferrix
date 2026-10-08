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
    "check-docs",
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
        "host-test" => host_test(),
        "host-doctest" => host_doctest(),
        "host-doc" => host_doc(),
        "check-apps" => apps(),
        "check-docs" => docs(),
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

    steps_at_once(AUDITS)?;
    step("terminal font", terminal_font)?;
    step("btrfs allocates fallibly", item_crates_allocate_fallibly)?;
    step("ARMv7-A ASID orders", armv7a_asid_orders)?;

    step("crate layering", || {
        let mut command = Command::new("bash");
        let _ = command
            .current_dir(&root)
            .arg("tools/common/check/check-crate-layering.sh");
        cargo::run(command, "tools/common/check/check-crate-layering.sh")
    })?;

    // The runtime and the native programs are freestanding like the kernel:
    // a `_start`, a panic handler, a linker script. The host can build none of
    // them, so they are linted per target below, and what of them can be
    // tested lives in `src/lib/proto/native`.
    step("clippy (host)", host_clippy)?;
    step("tests", host_test)?;
    step("doc tests", host_doctest)?;
    step("documentation", host_doc)?;
    loom(&root)?;

    compositor(&root)?;
    userland(&root)?;
    adbd(&root)?;
    nvos(&root)?;
    apps()?;

    if args.ferrousli {
        ferrousli(&root)?;
    }

    if args.zinc {
        zinc(&root)?;
    }

    // Opt-in, because it is minutes rather than seconds and needs a nightly
    // toolchain with the miri component: the same crates as CI's Miri job, so
    // a UB report can be reproduced before pushing. `cargo xtask miri` is the
    // same step without everything above it.
    if args.miri {
        miri_all(args.jobs)?;
    }

    if args.fast {
        println!("\nchecked (--fast: cross-target clippy skipped)");
        return Ok(());
    }

    cross_target_clippy()?;

    println!("\nchecked");
    Ok(())
}

/// ARMv7-A's ASID register sequences and F-67's remap order, held by
/// `tools/common/check/check-armv7a-asid.py`: orders no boot under QEMU can
/// show, since QEMU empties its TLB at every change of ASID and every `TTBCR`
/// write and shows no TLB conflict (`docs/OPAQUE-KERNEL.md` §9.13). Each of
/// `cpu.rs`'s three sequences is exactly its instructions, each of
/// `space.rs`'s three remaps calls `break_before_make` between its
/// `forget_in` and its `map_in`, and a space's tag is forgotten only in check
/// code.
///
/// Verifies: L.user.125
fn armv7a_asid_orders() -> Result<()> {
    python_with("tools/common/check/check-armv7a-asid.py", &[])
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
/// Verifies: L.armv7a.19
pub(crate) fn loom(root: &std::path::Path) -> Result<()> {
    let dir = root.join("src/tests/loom");
    let native = |arguments: &[&str]| {
        let mut command = Command::new(cargo_binary());
        let _ = command.current_dir(&dir).args(arguments);
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
        let _ = command.current_dir(&dir).args(arguments);
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

/// The Linux-ABI workspaces under `src/user/system/linux/` that the host steps do not
/// reach: init's, authentication's, the package manager's and media's.
fn userland(root: &std::path::Path) -> Result<()> {
    linux_workspace(root, "init", "src/user/system/linux/init")?;
    linux_workspace(root, "auth", "src/user/system/linux/auth")?;
    linux_workspace(root, "pkg", "src/user/system/linux/pkg")?;
    step("auth: no sabotage in the environment", no_sabotage)?;
    media(root)
}

/// The gates of every app under `src/user/apps/`, found by their manifests
/// (`docs/APPS.md` §5): nothing here names one. Each is a workspace of its
/// own, so the host steps above never reach it.
fn apps() -> Result<()> {
    let mut apps = Vec::new();
    step("apps: manifests", || {
        apps = crate::apps::discover()?;
        println!("  {} apps", apps.len());
        Ok(())
    })?;
    for app in &apps {
        let name = &app.recipe.package.name;
        step(
            &format!("app {name}: nothing outside its folder names it"),
            || crate::apps::stays_in_its_folder(app),
        )?;
        step(&format!("app {name}: formatting"), || {
            crate::apps::formatting(app)
        })?;
        step(&format!("app {name}: clippy and tests (host)"), || {
            crate::apps::host(app)
        })?;
        step(&format!("app {name}: clippy (targets)"), || {
            crate::apps::targets(app)
        })?;
    }
    Ok(())
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
        let _ = command.current_dir(&dir).args(arguments);
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
        let _ = command.current_dir(&dir).args(arguments);
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
        let _ = command.current_dir(&dir).args(arguments);
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
        let _ = command.current_dir(&dir).args(arguments);
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

/// The doc tests `--all-targets` skips: `cargo xtask host-doctest`.
pub(crate) fn host_doctest() -> Result<()> {
    host_cargo(&["test"], &["--doc"], &[], "cargo test --doc")
}

/// The documentation, warnings denied, as the lint config denies broken
/// intra-doc links: `cargo xtask host-doc`.
pub(crate) fn host_doc() -> Result<()> {
    host_cargo(
        &["doc"],
        &["--no-deps"],
        &[("RUSTDOCFLAGS", "-D warnings")],
        "cargo doc",
    )
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
    let started = Instant::now();
    let results: Vec<(Duration, Result<Vec<u8>>)> = std::thread::scope(|scope| {
        let running: Vec<_> = steps
            .iter()
            .map(|(_, scripts)| scope.spawn(move || scripts_in_turn(scripts)))
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
    for ((name, _), (spent, result)) in steps.iter().zip(results) {
        println!("\n== {name}");
        match result {
            Ok(said) => print!("{}", String::from_utf8_lossy(&said)),
            Err(error) => {
                println!("{error}");
                let _ = first_failure.get_or_insert(error);
            }
        }
        println!("-- {name}: {:.1} s", spent.as_secs_f64());
    }
    took("the steps above, at once", started);
    first_failure.map_or(Ok(()), Err)
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
fn kernel_clippy_both_settings() -> Result<()> {
    let mut with_defences = vec!["-p", "ferrix-kernel"];
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
