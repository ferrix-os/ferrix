//! `test-sem`: System V semaphores as a program uses them.
//!
//! The program is `src/tests/sem/`, built for each architecture's musl target --
//! with `--i686`, x86-64's is 32-bit x86, whose musl reaches the semaphores
//! through `ipc` (117), as the Steam client's glibc does -- and booted as
//! init. It forks children that contend for a `SEM_UNDO` mutex, kills one
//! holding it, waits for zero, and is interrupted and removed out of a
//! blocked `semop`, and reopens by key the sets an ended process made, as
//! the Steam client does when it restarts. It prints a line per step and `sem: all ok`, and exits
//! 0; this requires every one of those lines, in order, and the status.
//!
//! Then the negative control: the same program with the killed child's
//! decrement taken without `SEM_UNDO` must fail on the undo step. An undo
//! check that could not fail would pass that build too.

use std::path::{Path, PathBuf};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, native, qemu, shell};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-threads",
        help_at: 540,
        help: &[
            "test-threads  Boot threads-test as init and require std::thread, Mutex, mpsc and /proc's thread count (--i686: the x86-64",
            "              image runs a 32-bit x86 build of it)",
        ],
        run,
    },
    Task {
        name: "test-sem",
        help_at: 550,
        help: &[
            "test-sem      Boot sem-test as init and require System V semaphores across forks: a SEM_UNDO mutex, undo at a kill,",
            "              EINTR, EIDRM and timeouts (--i686: 32-bit x86, through ipc(117), as Steam calls them)",
        ],
        run,
    },
    Task {
        name: "test-pipewait",
        help_at: 560,
        help: &[
            "test-pipewait Boot pipewait-test as init and require a read blocked on a pipe to be ended by a caught signal",
            "              (EINTR, and a restart under SA_RESTART), SIGKILL, a stop and continue, and a freeze and thaw",
        ],
        run,
    },
    Task {
        name: "test-shm",
        help_at: 570,
        help: &[
            "test-shm      Boot shm-test as init and require System V shared memory as Chromium's MIT-SHM uses it: a segment",
            "              attached by two processes, removed while attached, gone at the last detach (--i686: 32-bit x86, through",
            "              ipc(117) and 395-398)",
        ],
        run,
    },
    Task {
        name: "test-container",
        help_at: 580,
        help: &[
            "test-container  Boot container-test as init and require stage 13's exit: uid 1000 runs a pid 1 in a user namespace",
            "              and six more, under a memory.max that reclaims and then OOM-kills it alone, with a seccomp filter",
            "              that blocks a call",
        ],
        run,
    },
    Task {
        name: "test-seccomp",
        help_at: 585,
        help: &[
            "test-seccomp  Boot seccomp-test as init and require seccomp-bpf as Chromium, bubblewrap and Flatpak use it: probes, errnos and chain",
            "              order, a trapped call emulated from its SIGSYS context, kills, strict mode, execve, TSYNC among threads being made",
            "              (--arch all runs the four ABIs: x86-64 and 32-bit x86 on the x86-64 kernel, AArch64, ARMv7-A; --i686: 32-bit x86 alone)",
        ],
        run,
    },
    Task {
        name: "test-seccomp-selftest",
        help_at: 586,
        help: &[
            "test-seccomp-selftest  Boot Linux v6.12's seccomp_bpf selftest as init (tools/common/fetch/fetch-seccomp-selftest.sh builds it)",
            "              and require every test to pass but those tools/common/data/seccomp-selftest/expected.txt names, as it names them",
        ],
        run,
    },
    Task {
        name: "test-sud",
        help_at: 587,
        help: &[
            "test-sud      Boot sud-test as init and require Syscall User Dispatch as Proton's Wine uses it: a range, a selector byte, the",
            "              SIGSYS a dispatched call raises and the context it gives back, kills, prctl's refusals, nothing inherited, seccomp after",
            "              it (x86-64 and 32-bit x86 on the x86-64 kernel; --i686: 32-bit x86 alone)",
        ],
        run,
    },
    Task {
        name: "test-procfs",
        help_at: 590,
        help: &[
            "test-procfs   Boot procfs-test as init and require /proc/self/fd links to stat as fstat (sockets, anonymous files, a pipe,",
            "              a memfd), /proc/net/tcp's inode to match, and every /proc inode number to fit 32 bits, and mincore (--i686: 32-bit x86)",
        ],
        run,
    },
    Task {
        name: "test-uvm",
        help_at: 600,
        help: &[
            "test-uvm      Build NVIDIA's UVM against uvm-kpi and ferrousli from the tree tools/common/fetch/fetch-nvidia.sh fetches",
            "              (or FERRIX_NVIDIA_SRC's), boot it as init, and require its 15 GPU-free self-tests to pass; then a krealloc",
            "              control that must fail one (x86_64)",
        ],
        run,
    },
    Task {
        name: "test-nvrm",
        help_at: 610,
        help: &[
            "test-nvrm     Build nvrm and its core, put the core on a volume, and boot nvrm as devmgr's Gpu driver on QEMU's",
            "              pci-testdev: handed over (marked, interrupts isolated, budget set, the core loaded, nvrm up with its",
            "              device), and refused for a budget too small",
            "              and for interrupts not isolated (x86_64)",
        ],
        run,
    },
    Task {
        name: "test-nvrm-link",
        help_at: 620,
        help: &[
            "test-nvrm-link",
            "              Build nvrm-link-test and its core from fetch-nvidia.sh's objects, run RM's no-GPU path (init,",
            "              root client, NV01_DEVICE_0 refused) on the host and on Ferrix from a volume, and each control",
            "              the core's loader must refuse (x86_64)",
        ],
        run,
    },
];

/// The Rust target the program is built for on `arch`: with `i686`, x86-64's
/// program is 32-bit x86, which the kernel runs in compatibility mode through
/// `int $0x80` (`docs/I386.md`, I4).
pub(crate) fn target(arch: Arch, i686: bool) -> Result<&'static str> {
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
    "sem: create ok",
    "sem: nowait ok",
    "sem: timeout ok",
    "sem: contention ok",
    "sem: undo ok",
    "sem: zero ok",
    "sem: eintr ok",
    "sem: eidrm ok",
    "sem: reopen ok",
    "sem: all ok",
];

/// The start of the line the negative control must fail with.
const NEGATIVE_FAILURE: &str =
    "sem: FAILED undo: a killed child's SEM_UNDO decrement was not undone";

/// Build `sem-test` for `arch`, as the negative control or not, and return
/// where the program is.
fn build(arch: Arch, negative: bool, i686: bool) -> Result<PathBuf> {
    let feature = negative.then_some("negative-control");
    build_test("sem", arch, feature, i686)
}

/// Build `src/tests/<name>`, the program `<name>-test`, for `arch` with `feature`
/// on if there is one, and return where the program is.
pub(crate) fn build_test(
    name: &str,
    arch: Arch,
    feature: Option<&str>,
    i686: bool,
) -> Result<PathBuf> {
    let target = target(arch, i686)?;
    let flavour = feature.unwrap_or("plain");
    let crate_name = format!("{name}-test");
    let target_dir = paths::target_dir().join(&crate_name).join(flavour);
    println!("  building {crate_name} ({flavour}) for {target}");
    let program = target_dir.join(target).join("release").join(&crate_name);
    let mut build = crate::builds::Build::cargo(
        format!("cargo build ({crate_name}, {flavour}) --target {target}"),
        paths::workspace_root().join("src").join("tests").join(name),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .output(&program);
    if let Some(feature) = feature {
        build = build.args(["--features", feature]);
    }
    // Ferrix's own `.cargo/config.toml` uses this triple for the ARMv7-A
    // loader, with its linker script, and cargo merges a parent directory's
    // flags for a target into `src/tests/<name>/`'s. The variable replaces every
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
pub(crate) fn boot(arch: Arch, program: &Path, args: &Args) -> Result<Vec<String>> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let image = fat::write_image(arch, &loader, &kernel, &natives, None)?;
    let lines = qemu::watch_then(arch, &image, &kernel, args, shell::EXITED, |_| Ok(()))?;
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default()
        .to_vec();
    Ok(after_boot)
}

/// The status init exited with, from the kernel's line.
pub(crate) fn status(lines: &[String]) -> Option<i32> {
    lines.iter().find_map(|line| {
        let at = line.find(shell::EXITED)?;
        line.get(at + shell::EXITED.len()..)?.trim().parse().ok()
    })
}

/// Whether `line` is `want`, once the serial log's time stamp is taken off.
pub(crate) fn says(line: &str, want: &str) -> bool {
    line.trim_end().ends_with(want)
}

/// Whether `line` holds `want` after its time stamp.
fn starts(line: &str, want: &str) -> bool {
    line.contains(want)
}

/// `test-sem`, and `test-threads`, `test-shm`, `test-container`, `test-seccomp`,
/// `test-seccomp-selftest`, `test-sud` and
/// `test-procfs`, the other musl programs booted as init, and `test-uvm`, `test-nvrm` and
/// `test-nvrm-link`, booted the
/// same way, which share its arm of `main`'s dispatch.
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    match command {
        "test-sem" => test_sem(args),
        "test-shm" => crate::shm::test_shm(args),
        "test-container" => crate::container::run(args),
        "test-pipewait" => crate::pipe_bench::test_pipewait(args),
        "test-seccomp" => crate::seccomp::test_seccomp(args),
        "test-seccomp-selftest" => crate::seccomp_selftest::run(args),
        "test-sud" => crate::sud::test_sud(args),
        "test-procfs" => crate::procfs::test_procfs(args),
        "test-uvm" => crate::uvm::test_uvm(args),
        "test-nvrm" => crate::nvrm::test_nvrm(args),
        "test-nvrm-link" => crate::nvrm_link::test_nvrm_link(args),
        _ => crate::threads::test_threads(args),
    }
}

/// The test on every architecture asked for.
fn test_sem(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let log = paths::build_dir(arch).join("serial.log");

        let program = build(arch, false, args.i686)?;
        let lines = boot(arch, &program, args)?;
        let mut remaining = lines.iter();
        for want in STEPS {
            if !remaining.any(|line| says(line, want)) {
                let failed = lines.iter().find(|line| line.contains("sem: FAILED"));
                return Err(Error::new(format!(
                    "{arch}: the semaphore test did not print `{want}` in order{}.\n  \
                     Serial output is in {}",
                    failed.map_or(String::new(), |line| format!("; it said `{}`", line.trim())),
                    log.display()
                )));
            }
        }
        if status(&lines) != Some(0) {
            return Err(Error::new(format!(
                "{arch}: the semaphore test did not exit with 0.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!(
            "  {arch}: semaphores contended across forks, undone at a kill, waited for zero, \
             interrupted and removed under their waiters, exit 0"
        );

        let negative = build(arch, true, args.i686)?;
        let lines = boot(arch, &negative, args)?;
        let failed_there = lines.iter().any(|line| starts(line, NEGATIVE_FAILURE));
        let passed_undo = lines.iter().any(|line| says(line, "sem: undo ok"));
        if !failed_there || passed_undo || status(&lines) != Some(1) {
            return Err(Error::new(format!(
                "{arch}: the negative control should fail with `{NEGATIVE_FAILURE}` and exit 1, \
                 and did not.\n  Serial output is in {}",
                log.display()
            )));
        }
        println!("  {arch}: the negative control failed on the undo step, as it must");
    }
    Ok(())
}
