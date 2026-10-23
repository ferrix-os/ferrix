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
use crate::task::Task;
use crate::workspace;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "check",
        help_at: 950,
        help: &[
            "check         Run every quality gate (fmt, clippy, layering, audits, tests, docs)",
        ],
        run: command,
    },
    Task {
        name: "miri",
        help_at: 1040,
        help: &[
            "miri          CI's Miri steps alone, --jobs crates at a time, building nothing else",
            "              (needs a nightly toolchain with miri)",
        ],
        run: command,
    },
    Task {
        name: "loom",
        help_at: 1050,
        help: &[
            "loom          check's loom models alone (src/tests/loom): the wait's and the way out's",
            "              orderings, each with a control that must fail",
        ],
        run: command,
    },
    Task {
        name: "check-ferrousli",
        help_at: 1030,
        help: &[
            "check-ferrousli",
            "              check --ferrousli's ferrousli steps alone, as CI runs them",
        ],
        run: command,
    },
    Task {
        name: "host-clippy",
        help_at: 960,
        help: &["host-clippy   check's host clippy step alone, as CI runs it"],
        run: command,
    },
    Task {
        name: "host-test",
        help_at: 970,
        help: &["host-test     check's host test step alone, as CI runs it"],
        run: command,
    },
    Task {
        name: "host-doctest",
        help_at: 980,
        help: &["host-doctest  check's doc test step alone, as CI runs it"],
        run: command,
    },
    Task {
        name: "host-doc",
        help_at: 990,
        help: &["host-doc      check's documentation step alone, as CI runs it"],
        run: command,
    },
    Task {
        name: "check-apps",
        help_at: 1120,
        help: &[
            "check-apps    check's steps for the apps alone: each app's formatting, clippy, tests and folder",
        ],
        run: command,
    },
    Task {
        name: "check-side",
        help_at: 1130,
        help: &[
            "check-side    check's steps for the workspaces of their own (loom, compositor, Linux programs, adbd, nvos, apps), which check runs beside its main workspace",
        ],
        run: command,
    },
    Task {
        name: "check-unit",
        help_at: 1140,
        help: &[
            "check-unit    one of check-side's units, the one FERRIX_CHECK_UNIT names, as the step cache runs it under strace (FERRIX_STEP_CACHE=on caches passed units by their inputs, docs/TEST-TIME.md)",
        ],
        run: command,
    },
    Task {
        name: "check-audit",
        help_at: 1150,
        help: &[
            "check-audit   one of check's audits, the one FERRIX_CHECK_UNIT names (audit:<name>), as the step cache runs it under strace keyed on the whole item",
        ],
        run: command,
    },
    Task {
        name: "check-docs",
        help_at: 1000,
        help: &[
            "check-docs    check's commit hooks, audits and generated-document checks alone, no cargo",
            "              step: the gate of a change to docs/ and top-level Markdown only",
        ],
        run: command,
    },
    Task {
        name: "check-ferrocene",
        help_at: 1010,
        help: &[
            "check-ferrocene",
            "              check's build of the kernel and the item's crates with Rust 1.95.0, the",
            "              upstream Rust of Ferrocene 26.05 (not Ferrocene's own compiler), alone",
        ],
        run: command,
    },
    Task {
        name: "native-clippy",
        help_at: 1060,
        help: &["native-clippy check's clippy of the native programs for --arch, as CI runs it"],
        run: |_, args| args.arches()?.into_iter().try_for_each(native_clippy),
    },
    Task {
        name: "model-doc",
        help_at: 1070,
        help: &["model-doc     Regenerate docs/generated/ from the SysML model"],
        run: |_, _| model_doc(),
    },
    Task {
        name: "test-flags",
        help_at: 196,
        help: &[
            "test-flags    test-boot, then the build flags' static checks: the profile rule, where flags",
            "              and boot options are read, the load ring's growth (docs/BUILD-FLAGS.md)",
        ],
        run: command,
    },
];

/// `check`, or one of its steps run alone, by the command's name
/// (the names [`TASKS`] gives it).
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
        "check-side" => {
            crate::orphans::exit_with_parent();
            side_workspaces(&paths::workspace_root())
        }
        "check-unit" => {
            crate::orphans::exit_with_parent();
            check_unit(&paths::workspace_root())
        }
        "check-audit" => check_audit(),
        "check-docs" => docs(),
        "check-ferrocene" => ferrocene_rust(),
        "test-flags" => test_flags(args),
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
    // The build flags (docs/BUILD-FLAGS.md): read in the kernel crate alone
    // (rule 6 b), and no boot option read that the registry does not
    // declare (BF4); and the gate's comparison of a kernel's `build` line
    // with its profile, self-tested here, since only a boot runs it whole.
    // The profile rule itself is the item boundary's, above.
    (
        "build flags",
        &[
            &["tools/common/check/check-build-flags.py"],
            &["tools/common/check/check-boot-line.py", "--self-test"],
        ],
    ),
    // The load ratchet (docs/roadmap/microkernel-m0.md, D): how much this
    // change grows what the microkernel switch must lift out of ring 0.
    // Reported, never failed: the customer allows load growth as now.
    (
        "load ring size",
        &[&["tools/common/check/check-load-ring.py"]],
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
    // The Intel GPU's forcewake, context-image and MOCS tables are generated
    // from i915's own files vendored beside them (docs/INTEL-GPU.md §3.6).
    // A table edited by hand is a register read without its power domain
    // awake, or a context image the engine restores wrongly, which on the
    // hardware is a hang with nothing in the log.
    (
        "intel gpu tables",
        &[&["tools/common/gen/gen-intel-xelp.py", "--check"]],
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
/// compositor's, the Linux programs', adbd's, nvos's and every app's:
/// `cargo xtask check-side`, which `check` starts beside its main
/// workspace's steps ([`Beside`]). [`SIDE_JOBS`] of them at once
/// ([`side_workspaces_at_once`]), or one after another when it is 1.
fn side_workspaces(root: &std::path::Path) -> Result<()> {
    match side_jobs()? {
        1 => {
            println!(
                "\n   the workspaces of their own, in turn ({SIDE_JOBS}={})",
                std::env::var(SIDE_JOBS).unwrap_or_default()
            );
            side_workspaces_in_turn(root)
        }
        jobs => side_workspaces_at_once(root, jobs),
    }
}

/// Set to how many of `check-side`'s units may run at once; 1 runs them one
/// after another in the order they always ran, as release runs and the
/// baseline do.
const SIDE_JOBS: &str = "FERRIX_SIDE_JOBS";

/// [`SIDE_JOBS`], or 4: the side workspaces were 464 s one after another on
/// a cold tree, the critical path of `check` (`docs/TEST-TIME.md`, *Cut
/// D2*), and four at once leave the main workspace's steps, which run
/// beside them, most of a 24-thread host.
fn side_jobs() -> Result<usize> {
    parse_side_jobs(std::env::var(SIDE_JOBS).ok().as_deref())
}

/// [`side_jobs`] from the variable's value.
fn parse_side_jobs(value: Option<&str>) -> Result<usize> {
    let Some(text) = value else {
        return Ok(4);
    };
    match text.trim().parse::<usize>() {
        Ok(jobs) if jobs >= 1 => Ok(jobs),
        _ => Err(Error::new(format!(
            "{SIDE_JOBS}={text}: give a number of units, 1 or more"
        ))),
    }
}

/// [`side_workspaces`] one after another, each in this process.
fn side_workspaces_in_turn(root: &std::path::Path) -> Result<()> {
    // loom's models are of the kernel's orderings: the item's, never cached
    // (the certification consultant, ledger line 1447).
    loom(root)?;
    let mut counts = Counts::default();
    counts.add(step_cache::unit(
        "compositor",
        &own_target_dir("compositor"),
        None,
        || compositor(root),
    )?);
    for (name, relative) in LINUX_WORKSPACES {
        counts.add(step_cache::unit(name, &own_target_dir(name), None, || {
            linux_workspace(root, name, relative)
        })?);
    }
    step("auth: no sabotage in the environment", no_sabotage)?;
    counts.add(step_cache::unit(
        "media",
        &own_target_dir("media"),
        None,
        || media(root),
    )?);
    counts.add(step_cache::unit(
        "adbd",
        &own_target_dir("adbd"),
        None,
        || adbd(root),
    )?);
    counts.add(step_cache::unit(
        "nvos",
        &own_target_dir("nvos"),
        None,
        || nvos(root),
    )?);
    apps(&mut counts)?;
    if step_cache::store().is_some() {
        println!(
            "\n-- the step cache: {} units ran, {} cached passes",
            counts.ran, counts.cached
        );
    }
    Ok(())
}

/// One of `check-side`'s units, as [`side_workspaces_at_once`] runs it.
#[derive(Debug)]
struct SideUnit {
    /// Its name, as `check-unit` and the step cache know it.
    name: String,
    /// Its own target directory.
    target: std::path::PathBuf,
    /// Whether the step cache may answer for it: not loom's, whose models
    /// are the item's orderings (the consultant, ledger line 1447).
    cacheable: bool,
    /// Its workspace's `Cargo.lock`, whose packages weigh it.
    lock: std::path::PathBuf,
    /// An app's: the directory its checks build in, told to its child.
    app_dir: Option<std::path::PathBuf>,
    /// Where it comes in the order the units always ran in.
    order: usize,
}

/// The units of `check-side` in the order they always ran, in lanes: a lane
/// runs its units one after another, lanes run at once. The apps share
/// target directories (`crate::apps::check_dirs`), and the apps of one
/// directory are one lane, by design: in turn, each finds what the one
/// before compiled. Every other unit is a lane of its own.
fn side_lanes(
    root: &std::path::Path,
    apps: &[crate::apps::App],
    dirs: &[std::path::PathBuf],
) -> Vec<Vec<SideUnit>> {
    let unit = |name: &str, target: std::path::PathBuf, dir: std::path::PathBuf| SideUnit {
        name: name.to_owned(),
        target,
        cacheable: name != "loom",
        lock: dir.join("Cargo.lock"),
        app_dir: None,
        order: 0,
    };
    let mut lanes = vec![
        vec![unit(
            "loom",
            own_target_dir("loom"),
            root.join("src/tests/loom"),
        )],
        vec![unit(
            "compositor",
            own_target_dir("compositor"),
            root.join("src/user/system/linux/compositor"),
        )],
    ];
    for (name, relative) in LINUX_WORKSPACES {
        lanes.push(vec![unit(name, own_target_dir(name), root.join(relative))]);
    }
    for (name, relative) in [
        ("media", "src/user/system/linux/media"),
        ("adbd", "src/user/system/linux/adbd"),
        ("nvos", "src/user/system/linux/drivers/nvrm/os/nvos"),
    ] {
        lanes.push(vec![unit(name, own_target_dir(name), root.join(relative))]);
    }
    // The apps of one shared directory are one lane, the directories'
    // lanes in the order their first app comes.
    let mut app_lanes: Vec<(&std::path::PathBuf, Vec<SideUnit>)> = Vec::new();
    // The order the units always ran in: each other unit is a lane of its
    // own, made in that order; the apps come after them, as found.
    for (order, lane) in lanes.iter_mut().enumerate() {
        for unit in lane {
            unit.order = order;
        }
    }
    let first_app = lanes.len();
    for (position, (app, dir)) in apps.iter().zip(dirs).enumerate() {
        let unit = SideUnit {
            app_dir: Some(dir.clone()),
            order: first_app + position,
            ..unit(
                &format!("app:{}", app.recipe.package.name),
                dir.clone(),
                app.dir.clone(),
            )
        };
        match app_lanes.iter_mut().find(|(lane, _)| *lane == dir) {
            Some((_, units)) => units.push(unit),
            None => app_lanes.push((dir, vec![unit])),
        }
    }
    lanes.extend(app_lanes.into_iter().map(|(_, units)| units));
    lanes
}

/// The packages in `lock`, a `Cargo.lock`: how much a unit compiles, which
/// a cold tree has no timings for; 0 when there is none.
fn packages_in(lock: &std::path::Path) -> usize {
    std::fs::read_to_string(lock)
        .map(|text| text.matches("[[package]]").count())
        .unwrap_or(0)
}

/// The order lanes start in: heaviest first, so the longest is not left to
/// start last; ties keep the order they always ran in.
fn lane_order(weights: &[usize]) -> Vec<usize> {
    let mut order: Vec<(usize, usize)> = weights.iter().copied().enumerate().collect();
    order.sort_by_key(|&(_, weight)| std::cmp::Reverse(weight));
    order.into_iter().map(|(lane, _)| lane).collect()
}

/// What one unit came to: how it ended, how long it took, its log.
#[derive(Debug)]
struct SideRun {
    outcome: Result<step_cache::Outcome>,
    took: Duration,
    log: std::path::PathBuf,
    /// What the log holds, read back when the unit ended.
    said: String,
}

/// [`side_workspaces`] `jobs` lanes at once (`docs/TEST-TIME.md`, *Cut
/// D2*). The steps that are not units run first, here, as they always ran.
/// Each unit then runs as a child, `cargo xtask check-unit` (under `strace`
/// when the step cache is on: `step_cache::unit_logged`), with its output
/// in `<target>/check-side/<unit>.log`. Every unit runs to its end whichever
/// fail; then each log is printed whole in the order the units always ran
/// in, and the first failure in that order is the error.
fn side_workspaces_at_once(root: &std::path::Path, jobs: usize) -> Result<()> {
    step("auth: no sabotage in the environment", no_sabotage)?;
    let (apps, dirs) = app_manifests()?;
    for app in &apps {
        let name = &app.recipe.package.name;
        // A search of the whole tree, so never a unit: it reads the item.
        step(
            &format!("app {name}: nothing outside its folder names it"),
            || crate::apps::stays_in_its_folder(app),
        )?;
    }
    let lanes = side_lanes(root, &apps, &dirs);
    let logs = paths::target_dir().join("check-side");
    std::fs::create_dir_all(&logs)?;
    let weights: Vec<usize> = lanes
        .iter()
        .map(|lane| lane.iter().map(|unit| packages_in(&unit.lock)).sum())
        .collect();
    let order = lane_order(&weights);
    let units: usize = lanes.iter().map(Vec::len).sum();
    println!(
        "\n== the workspaces of their own: {units} units in {} lanes, {jobs} at once \
         ({SIDE_JOBS}={}; 1 runs them in turn); each one's output follows when all have ended",
        lanes.len(),
        std::env::var(SIDE_JOBS).unwrap_or_else(|_| "unset".to_owned())
    );
    for &lane in &order {
        let (Some(units), Some(weight)) = (lanes.get(lane), weights.get(lane)) else {
            continue;
        };
        let names: Vec<&str> = units.iter().map(|unit| unit.name.as_str()).collect();
        println!("   lane ({weight} packages): {}", names.join(", "));
    }
    let next = std::sync::Mutex::new(order.into_iter());
    let done: std::sync::Mutex<Vec<(usize, Vec<SideRun>)>> = std::sync::Mutex::new(Vec::new());
    let died = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.min(lanes.len()))
            .map(|_| scope.spawn(|| side_worker(&next, &lanes, &logs, &done)))
            .collect();
        // A worker that panicked ends here, not in the scope: its lane's
        // units are then the ones with no end recorded, and every log is
        // still printed.
        workers
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .filter(std::result::Result::is_err)
            .count()
    });
    if died > 0 {
        println!("\n-- check-side: {died} worker(s) died");
    }
    let done = done
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (counts, failures) = side_verdicts(&lanes, done);
    if step_cache::store().is_some() {
        println!(
            "\n-- the step cache: {} units ran, {} cached passes",
            counts.ran, counts.cached
        );
    }
    if !failures.is_empty() {
        let names: Vec<&str> = failures.iter().map(|(name, _)| name.as_str()).collect();
        println!(
            "\n-- check-side: {} of {units} units failed: {}",
            failures.len(),
            names.join(", ")
        );
    }
    failures
        .into_iter()
        .next()
        .map_or(Ok(()), |(_, error)| Err(error))
}

/// Each unit's log printed and its outcome counted, in the order the units
/// always ran in, whatever order the lanes ended in (`done`); the failures in
/// that order, each with its unit's name. A unit with no run recorded fails.
fn side_verdicts(
    lanes: &[Vec<SideUnit>],
    mut done: Vec<(usize, Vec<SideRun>)>,
) -> (Counts, Vec<(String, Error)>) {
    done.sort_by_key(|(index, _)| *index);
    let mut runs = done.into_iter().peekable();
    // Each unit with its run, or none: then in the order the units always
    // ran in, whatever lane they were in.
    let mut units: Vec<(&SideUnit, Option<SideRun>)> = Vec::new();
    for (index, lane) in lanes.iter().enumerate() {
        let mut ran = match runs.next_if(|(lane, _)| *lane == index) {
            Some((_, ran)) => ran.into_iter(),
            None => Vec::new().into_iter(),
        };
        for unit in lane {
            units.push((unit, ran.next()));
        }
    }
    units.sort_by_key(|(unit, _)| unit.order);
    let mut counts = Counts::default();
    let mut failures = Vec::new();
    for (unit, run) in units {
        let Some(run) = run else {
            failures.push((
                unit.name.clone(),
                Error::new(format!("{}: no end recorded (its worker died)", unit.name)),
            ));
            continue;
        };
        print_side_run(unit, &run);
        match run.outcome {
            Ok(outcome) => counts.add(outcome),
            Err(error) => failures.push((unit.name.clone(), error)),
        }
    }
    (counts, failures)
}

/// Print one unit's log whole, under a line with its time, and its verdict.
fn print_side_run(unit: &SideUnit, run: &SideRun) {
    println!(
        "\n== {} ({:.1} s, {})",
        unit.name,
        run.took.as_secs_f64(),
        run.log.display()
    );
    print!("{}", run.said);
    let verdict = match &run.outcome {
        Ok(_) => "passed".to_owned(),
        Err(error) => format!("FAILED: {error}"),
    };
    println!(
        "-- {}: {verdict}, {:.1} s",
        unit.name,
        run.took.as_secs_f64()
    );
}

/// One of [`side_workspaces_at_once`]'s workers: take the next lane, run
/// its units in turn, keep what each came to, until no lane is left.
fn side_worker(
    next: &std::sync::Mutex<std::vec::IntoIter<usize>>,
    lanes: &[Vec<SideUnit>],
    logs: &std::path::Path,
    done: &std::sync::Mutex<Vec<(usize, Vec<SideRun>)>>,
) {
    loop {
        let Some(index) = lock(next).next() else {
            return;
        };
        let runs = lanes.get(index).map_or_else(Vec::new, |lane| {
            lane.iter().map(|unit| run_side_unit(unit, logs)).collect()
        });
        lock(done).push((index, runs));
    }
}

/// Run `unit` as a child with its output in `<logs>/<unit>.log`.
fn run_side_unit(unit: &SideUnit, logs: &std::path::Path) -> SideRun {
    let log = logs.join(format!("{}.log", unit.name.replace(':', "-")));
    let began = Instant::now();
    let mut outcome = std::fs::File::create(&log)
        .map_err(|error| Error::new(format!("creating {}: {error}", log.display())))
        .and_then(|file| {
            step_cache::unit_logged(
                &unit.name,
                &unit.target,
                unit.app_dir.as_deref(),
                &file,
                unit.cacheable,
            )
        });
    let took = began.elapsed();
    let said = match std::fs::read(&log) {
        Ok(said) => String::from_utf8_lossy(&said).into_owned(),
        Err(error) => {
            // A unit whose log cannot be read back fails, whatever it said.
            let error = Error::new(format!("{}: reading {}: {error}", unit.name, log.display()));
            outcome = outcome.and(Err(error));
            String::new()
        }
    };
    SideRun {
        outcome,
        took,
        log,
        said,
    }
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
    ("dhcp", "src/user/system/linux/dhcp"),
    ("dmesg", "src/user/system/linux/dmesg"),
    ("installer", "src/user/system/linux/installer"),
];

/// `cargo xtask check-unit`: the one unit `FERRIX_CHECK_UNIT` names, as
/// [`step_cache`] runs it under `strace`.
fn check_unit(root: &std::path::Path) -> Result<()> {
    let name = std::env::var(step_cache::UNIT_VARIABLE)
        .map_err(|_| Error::new("check-unit: FERRIX_CHECK_UNIT names no unit"))?;
    let apps = if name.starts_with("app:") {
        crate::apps::discover()?
    } else {
        Vec::new()
    };
    match resolve_unit(&name, &apps) {
        Some(Unit::Loom) => loom(root),
        Some(Unit::Compositor) => compositor(root),
        Some(Unit::Linux(unit, relative)) => linux_workspace(root, unit, relative),
        Some(Unit::Media) => media(root),
        Some(Unit::Adbd) => adbd(root),
        Some(Unit::Nvos) => nvos(root),
        Some(Unit::App(app)) => {
            // `check-side` works out which shared directory the app builds
            // in and says so: no `cargo metadata` and no scan of the apps here.
            let dir = std::env::var_os(step_cache::APP_DIR_VARIABLE).ok_or_else(|| {
                Error::new(format!(
                    "check-unit {name}: {} names no directory to build in",
                    step_cache::APP_DIR_VARIABLE
                ))
            })?;
            app_steps(app, std::path::Path::new(&dir))
        }
        None => Err(Error::new(format!("check-unit: no unit {name}"))),
    }
}

/// What a `check-side` unit's name stands for.
#[derive(Debug)]
enum Unit<'a> {
    Loom,
    Compositor,
    Linux(&'static str, &'static str),
    Media,
    Adbd,
    Nvos,
    App(&'a crate::apps::App),
}

/// The unit `name` names, from the lists [`side_lanes`] is made from.
fn resolve_unit<'a>(name: &str, apps: &'a [crate::apps::App]) -> Option<Unit<'a>> {
    if let Some(app) = name.strip_prefix("app:") {
        return apps
            .iter()
            .find(|found| found.recipe.package.name == app)
            .map(Unit::App);
    }
    match name {
        "loom" => Some(Unit::Loom),
        "compositor" => Some(Unit::Compositor),
        "media" => Some(Unit::Media),
        "adbd" => Some(Unit::Adbd),
        "nvos" => Some(Unit::Nvos),
        _ => LINUX_WORKSPACES
            .iter()
            .find(|(unit, _)| *unit == name)
            .map(|(unit, relative)| Unit::Linux(unit, relative)),
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
///
/// The kernel crate builds only with its build flags (`src/kernel/build.rs`
/// refuses a build without them), so the step runs once for each certified
/// profile that builds today -- `present` until M5 -- with that profile's
/// flags as xtask's resolver gives them (the certification consultant,
/// ledger 1039, condition 1). A profile refused by `lands` is not built, so
/// it is not type-checked either; none at all is a failure.
fn ferrocene_rust() -> Result<()> {
    let mut checked = 0_usize;
    for profile in crate::flags::profile_names()? {
        let resolved = match crate::flags::resolve(Some(&profile), &[]) {
            Ok(resolved) if resolved.level != "none" => resolved,
            _ => continue,
        };
        println!("   -- profile {profile}: {}", resolved.cfg_words());
        python_env(
            "tools/common/check/check-ferrocene-rust.py",
            &[],
            &cargo::flag_environment(&resolved),
        )?;
        checked += 1;
    }
    if checked == 0 {
        return Err(Error::new(
            "no certified profile builds today, so none was type-checked with Ferrocene's Rust",
        ));
    }
    Ok(())
}

/// `test-flags`: the build flags' checks in one command, which their negative
/// controls run under `gate.sh control` (`docs/BUILD-FLAGS.md` §6): build
/// and boot `--kernel-profile` on each `--arch`, as `test-boot` does, whose
/// gate compares the kernel's `build` line with the profile file; then the
/// static checks -- the profile rule (`check-item-boundary.py`), where flags
/// and boot options are read (`check-build-flags.py`) and the load ratchet
/// (`check-load-ring.py`, failing on growth only with `--fail-on-growth`).
/// The boot comes first, so a control of a static check shows a guest that
/// started before the check fired. Every static check runs whichever fails.
fn test_flags(args: &Args) -> Result<()> {
    crate::test_boot(args)?;
    let growth: &[&str] = if args.fail_on_growth {
        &["--fail-on-growth"]
    } else {
        &[]
    };
    let checks: [(&str, &[&str]); 3] = [
        ("tools/common/check/check-item-boundary.py", &[]),
        ("tools/common/check/check-build-flags.py", &[]),
        ("tools/common/check/check-load-ring.py", growth),
    ];
    let failed: Vec<String> = checks
        .iter()
        .filter_map(|(script, arguments)| python_with(script, arguments).err())
        .map(|error| error.to_string())
        .collect();
    if failed.is_empty() {
        println!("test-flags: the boot's build line and every static flag check passed");
        Ok(())
    } else {
        Err(Error::new(format!("test-flags: {}", failed.join("; "))))
    }
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
    // unlinted until CI. The kernel for every target is one cargo call per
    // build, and the kernel `mitigations=off` another in its own target
    // directory, so the two run at once (`docs/TEST-TIME.md`, C1).
    step(
        "clippy (kernel, every architecture: each buildable profile, checks=off, mitigations=off, linux_abi=off)",
        kernel_clippy_every_build,
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

/// `check-side`'s step "apps: manifests": every app, and the target
/// directory its `check` steps build in (`crate::apps::check_dirs`).
fn app_manifests() -> Result<(Vec<crate::apps::App>, Vec<std::path::PathBuf>)> {
    let mut found = (Vec::new(), Vec::new());
    step("apps: manifests", || {
        let apps = crate::apps::discover()?;
        let dirs = crate::apps::check_dirs(&apps)?;
        let mut distinct = dirs.clone();
        distinct.sort();
        distinct.dedup();
        println!(
            "  {} apps, their checks in {} shared target directories",
            apps.len(),
            distinct.len()
        );
        found = (apps, dirs);
        Ok(())
    })?;
    Ok(found)
}

/// The gates of every app under `src/user/apps/`, found by their manifests
/// (`docs/APPS.md` §5): nothing here names one. Each is a workspace of its
/// own, so the host steps above never reach it.
fn apps(counts: &mut Counts) -> Result<()> {
    let (apps, dirs) = app_manifests()?;
    for (app, dir) in apps.iter().zip(&dirs) {
        let name = &app.recipe.package.name;
        // A search of the whole tree, so never a unit: it reads the item.
        step(
            &format!("app {name}: nothing outside its folder names it"),
            || crate::apps::stays_in_its_folder(app),
        )?;
        counts.add(step_cache::unit(
            &format!("app:{name}"),
            dir,
            Some(dir),
            || app_steps(app, dir),
        )?);
    }
    Ok(())
}

/// One app's formatting, clippy and tests.
fn app_steps(app: &crate::apps::App, dir: &std::path::Path) -> Result<()> {
    let name = &app.recipe.package.name;
    step(&format!("app {name}: formatting"), || {
        crate::apps::formatting(app, dir)
    })?;
    step(&format!("app {name}: clippy and tests (host)"), || {
        crate::apps::host(app, dir)
    })?;
    step(&format!("app {name}: clippy (targets)"), || {
        crate::apps::targets(app, dir)
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

/// `cargo clippy` over the kernel for every architecture, once for each
/// kernel build that can be made today, which is what `check` "builds"
/// them with: every named profile that builds (`present`, `desktop`), and the
/// two ad-hoc builds that must not rot (`docs/BUILD-FLAGS.md` §6):
/// `checks=off` (BF7), `mitigations=off` and `linux_abi=off`. A profile refused by `lands`
/// (`max`, `fast` before M5) is reported as not yet buildable, not as a
/// failure; one that does not resolve at all fails.
///
/// `mitigations=off` changes codegen for every crate, so it is linted in the
/// target directory `cargo::kernel` builds it into, beside the others: no
/// cache is thrown away for another's, and cargo's lock on one directory
/// never waits for the other. The rest differ only in the kernel crate's
/// `cfg`s, so each relints that crate alone, one after another.
///
/// Every one of the kernel's packages is linted: the binary and whatever
/// library crates sit beneath `src/kernel` (`docs/KERNEL-CRATES.md`), as
/// `workspace::members` finds them, so a crate split out of the kernel
/// cannot leave the lint pass by being missed from a list.
fn kernel_clippy_every_build() -> Result<()> {
    let packages = workspace::members(&paths::workspace_root())?.kernel;
    if !packages.iter().any(|name| name == "ferrix-kernel") {
        return Err(Error::new(
            "the workspace has no ferrix-kernel package under src/kernel to lint",
        ));
    }
    let mut every_arch: Vec<&str> = Vec::new();
    for package in &packages {
        every_arch.extend(["-p", package.as_str()]);
    }
    for arch in Arch::ALL {
        every_arch.extend(["--target", arch.kernel_target()]);
    }
    let mut builds = Vec::new();
    for profile in crate::flags::profile_names()? {
        match crate::flags::resolve(Some(&profile), &[]) {
            Ok(resolved) => builds.push((format!("profile {profile}"), resolved)),
            Err(why) => {
                // Refused by `lands` only: one that does not resolve at all
                // is red.
                let _ = crate::flags::resolve_unbuilt(Some(&profile), &[])?;
                println!("   {why}: not yet buildable, validated statically");
            }
        }
    }
    let ad_hoc = |flag: &str| {
        crate::flags::resolve(None, &[(flag.to_owned(), "off".to_owned())])
            .map(|resolved| (format!("ad hoc, {flag}=off"), resolved))
    };
    builds.push(ad_hoc("checks")?);
    // Without the Linux ABI (`docs/LINUX-ABI-SEPARATION.md` §11.11): the
    // flag and its placement together, since `linux_abi=off` requires
    // `personality=none` and only `fs=none` goes with that.
    let without_linux: Vec<(String, String)> = [
        ("linux_abi", "off"),
        ("personality", "none"),
        ("fs", "none"),
    ]
    .iter()
    .map(|(flag, value)| ((*flag).to_owned(), (*value).to_owned()))
    .collect();
    builds.push((
        "ad hoc, linux_abi=off".to_owned(),
        crate::flags::resolve(None, &without_linux)?,
    ));
    let (_, off) = ad_hoc("mitigations")?;

    let configs: Vec<String> = Arch::ALL
        .iter()
        .map(|arch| cargo::mitigations_off_config(arch.kernel_target()))
        .collect();
    let directory = cargo::mitigations_off_target_dir();
    let directory = directory.to_string_lossy();
    let mut without = every_arch.clone();
    for config in &configs {
        without.extend(["--config", config]);
    }
    without.extend(["--target-dir", &directory]);
    // The off build is captured and printed after the rest, so the passes'
    // diagnostics never interleave.
    let (first, second) = std::thread::scope(|scope| {
        let off = scope.spawn(|| clippy_captured(&without, &cargo::flag_environment(&off)));
        let mut on = Ok(());
        for (name, resolved) in &builds {
            println!("\n   -- {name}: {}", resolved.cfg_words());
            on = clippy_with(&every_arch, &cargo::flag_environment(resolved));
            if on.is_err() {
                break;
            }
        }
        let off = off
            .join()
            .unwrap_or_else(|_| Err(Error::new("the mitigations=off clippy thread panicked")));
        (on, off)
    });
    println!("\n   -- ad hoc, mitigations=off: {}", off.cfg_words());
    second?;
    first
}

/// [`clippy`], with the kernel's flags in its environment and its output
/// kept and printed whole when it ends.
fn clippy_captured(arguments: &[&str], flags: &[(&str, String)]) -> Result<()> {
    let output = Command::new(cargo_binary())
        .current_dir(paths::workspace_root())
        .arg("clippy")
        .args(arguments)
        .args(["--", "-D", "warnings"])
        .envs(flags.iter().map(|(key, value)| (*key, value)))
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::new(format!("could not run cargo clippy: {error}")))?;
    print!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    cargo::finished(output.status, "cargo clippy, mitigations=off")
}

/// [`clippy`], with the kernel's flags in its environment.
fn clippy_with(arguments: &[&str], flags: &[(&str, String)]) -> Result<()> {
    let mut command = Command::new(cargo_binary());
    let _ = command
        .current_dir(paths::workspace_root())
        .arg("clippy")
        .args(arguments)
        .args(["--", "-D", "warnings"])
        .envs(flags.iter().map(|(key, value)| (*key, value)));
    cargo::run(command, "cargo clippy")
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
    python_env(script, arguments, &[])
}

/// [`python_with`], with `environment` added to the script's environment.
pub(crate) fn python_env(
    script: &str,
    arguments: &[&str],
    environment: &[(&str, String)],
) -> Result<()> {
    let interpreter = python_interpreter().ok_or_else(|| {
        Error::new("no working Python interpreter on PATH (tried python3, python)")
    })?;

    let mut command = Command::new(interpreter);
    let _ = command
        .current_dir(paths::workspace_root())
        .arg(script)
        .args(arguments)
        .envs(environment.iter().map(|(key, value)| (*key, value)));
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
    fn side_units_run_heaviest_lane_first_and_the_apps_share_one_lane() {
        assert_eq!(parse_side_jobs(None).unwrap(), 4);
        assert_eq!(parse_side_jobs(Some("1")).unwrap(), 1);
        assert_eq!(parse_side_jobs(Some(" 6 ")).unwrap(), 6);
        assert!(parse_side_jobs(Some("0")).is_err());
        assert!(parse_side_jobs(Some("all")).is_err());

        // Heaviest first; equal weights keep their order.
        assert_eq!(lane_order(&[31, 126, 14, 126, 0]), vec![1, 3, 0, 2, 4]);

        let root = paths::workspace_root();
        let apps = crate::apps::discover().unwrap();
        assert!(apps.len() >= 3);
        // The second app in a directory of its own, the others in one.
        let shared = std::path::PathBuf::from("/shared");
        let apart = std::path::PathBuf::from("/apart");
        let dirs: Vec<std::path::PathBuf> = (0..apps.len())
            .map(|index| {
                if index == 1 {
                    apart.clone()
                } else {
                    shared.clone()
                }
            })
            .collect();
        let lanes = side_lanes(&root, &apps, &dirs);
        let names: Vec<Vec<&str>> = lanes
            .iter()
            .map(|lane| lane.iter().map(|unit| unit.name.as_str()).collect())
            .collect();
        // The order they always ran in: loom, the compositor, the Linux
        // workspaces, media, adbd, nvos, then the apps, a lane for each
        // directory in the order its first app comes.
        let mut expected: Vec<Vec<String>> = vec![vec!["loom".into()], vec!["compositor".into()]];
        for (name, _) in LINUX_WORKSPACES {
            expected.push(vec![(*name).to_owned()]);
        }
        for name in ["media", "adbd", "nvos"] {
            expected.push(vec![name.to_owned()]);
        }
        let app_name = |app: &crate::apps::App| format!("app:{}", app.recipe.package.name);
        expected.push(
            apps.iter()
                .enumerate()
                .filter(|(index, _)| *index != 1)
                .map(|(_, app)| app_name(app))
                .collect(),
        );
        expected.push(apps.get(1).map(app_name).into_iter().collect());
        assert_eq!(names, expected);
        for unit in lanes.iter().flatten() {
            // Loom's models are the item's: never cached (ledger 1447).
            assert_eq!(unit.cacheable, unit.name != "loom", "{}", unit.name);
            if unit.name.starts_with("app:") {
                let second = apps.get(1).map(app_name).unwrap_or_default();
                let dir = if unit.name == second { &apart } else { &shared };
                assert_eq!(&unit.target, dir, "{}", unit.name);
            } else {
                assert_eq!(unit.target, own_target_dir(&unit.name));
            }
        }
        // Their order is the order they always ran in, apps as found.
        let mut by_order: Vec<(usize, &str)> = lanes
            .iter()
            .flatten()
            .map(|unit| (unit.order, unit.name.as_str()))
            .collect();
        by_order.sort_unstable();
        let flat: Vec<String> = expected
            .iter()
            .take(expected.len() - 2)
            .flatten()
            .cloned()
            .chain(apps.iter().map(app_name))
            .collect();
        assert_eq!(
            by_order.iter().map(|(_, name)| *name).collect::<Vec<_>>(),
            flat.iter().map(String::as_str).collect::<Vec<_>>()
        );
        // Each workspace's lock file is found, so weights mean something.
        assert!(packages_in(&root.join("src/user/system/linux/compositor/Cargo.lock")) > 50);
        assert_eq!(packages_in(&root.join("no/such/Cargo.lock")), 0);
    }

    #[test]
    fn every_side_unit_resolves_in_check_unit() {
        let root = paths::workspace_root();
        let apps = crate::apps::discover().unwrap();
        let dirs = crate::apps::check_dirs(&apps).unwrap();
        for unit in side_lanes(&root, &apps, &dirs).iter().flatten() {
            assert!(
                resolve_unit(&unit.name, &apps).is_some(),
                "check-unit cannot run {}",
                unit.name
            );
        }
        assert!(resolve_unit("app:no-such-app", &apps).is_none());
        assert!(resolve_unit("nothing", &apps).is_none());
    }

    #[test]
    fn side_failures_are_every_one_in_the_order_the_units_always_ran() {
        let unit = |name: &str, order: usize| SideUnit {
            name: name.to_owned(),
            target: std::path::PathBuf::new(),
            cacheable: true,
            lock: std::path::PathBuf::new(),
            app_dir: None,
            order,
        };
        let run = |fails: bool| SideRun {
            outcome: if fails {
                Err(Error::new("failed"))
            } else {
                Ok(step_cache::Outcome::Ran)
            },
            took: Duration::ZERO,
            log: std::path::PathBuf::new(),
            said: String::new(),
        };
        // The apps a, b, c, d always ran in that order; b and d share a
        // directory, so a lane, after a and c's.
        let lanes = vec![
            vec![unit("loom", 0)],
            vec![unit("compositor", 1)],
            vec![unit("init", 2)],
            vec![unit("app:a", 3), unit("app:c", 5)],
            vec![unit("app:b", 4), unit("app:d", 6)],
        ];
        // Ended in another order than they always ran in, the apps' lanes
        // first; the compositor, app c and app b failed, and lane 2 never
        // ended.
        let done = vec![
            (4, vec![run(true), run(false)]),
            (3, vec![run(false), run(true)]),
            (1, vec![run(true)]),
            (0, vec![run(false)]),
        ];
        let (counts, failures) = side_verdicts(&lanes, done);
        let names: Vec<&str> = failures.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["compositor", "init", "app:b", "app:c"]);
        assert_eq!(counts.ran, 3);
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
