//! Structural coverage of the kernel: where each boot's trace goes, and
//! `cargo xtask coverage`, the suite whose union is the certified item's
//! statement coverage on one architecture (`docs/certification/
//! VERIFICATION.md` §3).
//!
//! The measurement is QEMU's `drcov` TCG plugin, named by
//! `FERRIX_QEMU_PLUGIN`, and `tools/common/gen/coverage-report.py`, which reads the
//! trace against the kernel's DWARF line table; `tools/common/gen/decision-coverage.py`
//! reads the same traces for decision coverage (§3.6). Two things about a suite of
//! gates make that harder than one boot:
//!
//! * **A gate may boot more than once**, and the plugin truncates the file it
//!   is given every time QEMU starts. `test-shell` boots four times, and
//!   `test-powerfail` twice per seed; each used to leave only its last
//!   boot's trace. Every boot after the first in one run of this program
//!   therefore writes a numbered file beside the first: `shell.drcov`,
//!   `shell.2.drcov`, `shell.3.drcov`.
//! * **Gates build different kernels.** `test-shell` builds its program in,
//!   `test-vfs` and `test-net` their command lists, and an address in one
//!   build is not the same statement in another. So each trace is kept with
//!   the ELF its boot ran: a copy named by its content, shared by every trace
//!   of the same build, and a `<trace>.kernel` file naming it, which the
//!   report reads.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// Boots started by this process with a plugin, which numbers their traces.
static BOOTS: AtomicU32 = AtomicU32::new(0);

/// The `-plugin` value the latest boot was given, numbered trace and all.
static LATEST: Mutex<Option<String>> = Mutex::new(None);

/// The `-plugin` value for the next boot: `plugin` with its trace numbered,
/// and the kernel it runs kept beside the trace.
///
/// # Errors
///
/// When the kernel cannot be read or kept.
pub(crate) fn plugin_for_boot(plugin: &str, kernel: Option<&Path>) -> Result<String> {
    let boot = BOOTS.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    let (numbered, trace) = numbered(plugin, boot);
    if let Some(trace) = &trace {
        let sidecar = sidecar(trace);
        match kernel {
            Some(kernel) => {
                let kept = keep_kernel(trace, kernel)?;
                std::fs::write(&sidecar, format!("{kept}\n"))?;
            }
            // A stale one would pair this boot's trace with another build.
            None => {
                let _ = std::fs::remove_file(&sidecar);
            }
        }
        println!("  coverage: this boot's trace is {}", trace.display());
    }
    if let Ok(mut latest) = LATEST.lock() {
        *latest = Some(numbered.clone());
    }
    Ok(numbered)
}

/// The `-plugin` value the latest boot was given, if it was given one: what
/// `crate::kaslr::record_coverage_slide` writes the boot's slide beside. Not
/// `FERRIX_QEMU_PLUGIN` itself, which names only the first boot's trace.
pub(crate) fn latest_plugin() -> Option<String> {
    LATEST.lock().ok().and_then(|latest| latest.clone())
}

/// `plugin` with the trace of boot `boot` numbered, and that trace's path.
///
/// The first boot keeps the name it was given, so a single boot's trace is
/// where the person running it said; the `n`th puts `.n` before the
/// extension. A plugin with no `filename=` writes where its own default says,
/// and is passed on unchanged.
fn numbered(plugin: &str, boot: u32) -> (String, Option<PathBuf>) {
    let mut trace = None;
    let parts: Vec<String> = plugin
        .split(',')
        .map(|part| {
            let Some(name) = part.strip_prefix("filename=") else {
                return part.to_owned();
            };
            let path = numbered_path(Path::new(name), boot);
            let rewritten = format!("filename={}", path.display());
            trace = Some(path);
            rewritten
        })
        .collect();
    (parts.join(","), trace)
}

/// `path` for boot `boot`: unchanged for the first, `stem.n.ext` after.
fn numbered_path(path: &Path, boot: u32) -> PathBuf {
    if boot <= 1 {
        return path.to_path_buf();
    }
    let stem = path
        .file_stem()
        .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
    let name = match path.extension() {
        Some(extension) => format!("{stem}.{boot}.{}", extension.to_string_lossy()),
        None => format!("{stem}.{boot}"),
    };
    path.with_file_name(name)
}

/// Where the name of a trace's kernel is written.
fn sidecar(trace: &Path) -> PathBuf {
    let mut name = trace.as_os_str().to_owned();
    name.push(".kernel");
    PathBuf::from(name)
}

/// Copy `kernel` beside `trace` under a name made from its content, unless a
/// trace of the same build already did, and answer that name.
fn keep_kernel(trace: &Path, kernel: &Path) -> Result<String> {
    let bytes = std::fs::read(kernel)
        .map_err(|error| Error::new(format!("reading {}: {error}", kernel.display())))?;
    let digest = crate::sha256::hex(&bytes);
    let name = format!("ferrix-kernel-{}.elf", digest.get(..16).unwrap_or(&digest));
    let directory = trace.parent().unwrap_or_else(|| Path::new("."));
    let kept = directory.join(&name);
    if !kept.is_file() {
        std::fs::create_dir_all(directory)?;
        std::fs::write(&kept, &bytes)?;
    }
    Ok(name)
}

/// One gate of the suite.
#[derive(Debug)]
struct Gate {
    /// The xtask command.
    command: &'static str,
    /// What its traces are called: the command without `test-`, unless one
    /// command runs twice.
    name: &'static str,
    /// Whether it needs `--init`, a static busybox.
    userland: bool,
    /// The one architecture it counts on, if it is only one: the gate refuses
    /// the others, fails on them for a reason [`SUITE`] gives, or is a second
    /// machine for that architecture.
    only: Option<Arch>,
    /// `FERRIX_ARM_MACHINE`, for a boot on a different Arm machine.
    arm_machine: Option<&'static str>,
    /// `FERRIX_ARM_CPU`, for a boot on a different Arm processor.
    arm_cpu: Option<&'static str>,
    /// `--reset`: end in a reset, not a power-off.
    reset: bool,
    /// Anything else the boot's environment needs: `FERRIX_X86_MACHINE`
    /// and `FERRIX_X86_CPU` for a different PC.
    env: &'static [(&'static str, &'static str)],
    /// Arguments the gate is given after the suite's own, such as `--smp 1`.
    extra: &'static [&'static str],
}

impl Gate {
    /// A gate on every architecture.
    const fn new(command: &'static str, name: &'static str, userland: bool) -> Gate {
        Gate {
            command,
            name,
            userland,
            only: None,
            arm_machine: None,
            arm_cpu: None,
            reset: false,
            env: &[],
            extra: &[],
        }
    }

    /// The same gate, on `arch` alone.
    const fn only(self, arch: Arch) -> Gate {
        Gate {
            only: Some(arch),
            ..self
        }
    }

    /// The same gate, on the Arm machine `machine` adds to `virt`.
    const fn on_machine(self, machine: &'static str) -> Gate {
        Gate {
            arm_machine: Some(machine),
            ..self
        }
    }

    /// The same gate, on the Arm processor model `cpu`.
    const fn on_cpu(self, cpu: &'static str) -> Gate {
        Gate {
            arm_cpu: Some(cpu),
            ..self
        }
    }

    /// The same gate, ending in a reset: `--reset`.
    const fn resetting(self) -> Gate {
        Gate {
            reset: true,
            ..self
        }
    }

    /// The same gate, with `env` in its environment.
    const fn with_env(self, env: &'static [(&'static str, &'static str)]) -> Gate {
        Gate { env, ..self }
    }

    /// The same gate, given `extra` as well.
    const fn with_args(self, extra: &'static [&'static str]) -> Gate {
        Gate { extra, ..self }
    }
}

/// Every boot gate that exercises the item, passes under the plugin, and
/// ends with a trace.
///
/// `test-boot` first: its kernel is the plain build, and the report counts
/// statements against it. Left out, and why:
///
/// * `test-seat` and `test-compositor`: the plugin slows TCG enough that the
///   first misses its redraw and the second tripped the TLB shootdown's
///   bound (`FERRIX-PANIC processor 0 never flushed its TLB for a shootdown`)
///   while a wait had one bound; not run since it has a late and a stuck one.
///   Both pass under TCG without it. A failing run is not coverage evidence.
/// * `test-foot`, `test-video`, `test-vkgears`, `test-rustc`, `test-chrome`
///   and `test-selfhost`: a GL host, ports or volumes fetched from outside
///   the tree.
const SUITE: &[Gate] = &[
    Gate::new("test-boot", "boot", false),
    // `virt` presents a GICv2 by default. The GICv3 and its ITS are the
    // Pixel 7's, and a boot on a `virt` that has them is the only run that
    // reaches that driver.
    Gate::new("test-boot", "boot-gicv3", false)
        .only(Arch::AArch64)
        .on_machine("gic-version=3"),
    // The Pixel 7 has no ACPI: the kernel finds its interrupt controller,
    // timer, processors and PSCI conduit in the device tree, which a `virt`
    // with ACPI never makes it read. Once with the GICv2 and its v2m frame,
    // and once as the phone is -- a GICv3 and ITS, on a processor with PAN,
    // RNDR, SSBS and the rest of what `cortex-a72` lacks and its cores have.
    Gate::new("test-boot", "boot-dt", false)
        .only(Arch::AArch64)
        .on_machine("acpi=off"),
    Gate::new("test-boot", "boot-dt-gicv3", false)
        .only(Arch::AArch64)
        .on_machine("acpi=off,gic-version=3")
        .on_cpu("max"),
    // The reference ARMv7-A core is a Cortex-A7, which Arm lists as affected
    // by no Spectre variant, so the switch barrier the kernel keeps for the
    // cores that are is never issued on it. A Cortex-A15 is one of those, and
    // the one other ARMv7-A core `virt` takes: the A8 and A9 have no generic
    // timer.
    Gate::new("test-boot", "boot-a15", false)
        .only(Arch::Armv7a)
        .on_cpu("cortex-a15"),
    // Every other boot ends in a power-off, so the reset a program's
    // `reboot` or `ferrix.onexit=reset` asks for was never taken on Arm:
    // PSCI's SYSTEM_RESET, after the console has drained. The gate requires
    // the loader to start again.
    Gate::new("test-boot", "boot-reset", false)
        .only(Arch::AArch64)
        .resetting(),
    Gate::new("test-boot", "boot-reset", false)
        .only(Arch::Armv7a)
        .resetting(),
    // `nosmp` keeps the boot processor alone, the first thing to try on a
    // machine whose second core is in doubt, and every other boot starts
    // them all. AArch64 reads it from the loader's command line; ARMv7-A
    // reads it from U-Boot's `bootargs`, which QEMU's U-Boot leaves empty.
    Gate::new("test-boot", "boot-nosmp", false)
        .only(Arch::AArch64)
        .with_args(&["--kernel-option", "nosmp"]),
    // ARMv7-A on one processor, as the suite's `--smp 2` never is: the
    // coherency report for a machine with nothing to be coherent with.
    Gate::new("test-boot", "boot-single", false)
        .only(Arch::Armv7a)
        .with_args(&["--smp", "1"]),
    // ARMv7-A with 3 GiB, so firmware loads the loader and the kernel above
    // the split between the halves of the address space, as the DK1's DDR
    // at 3 GiB always is: the loader's alias of itself in the kernel's tree
    // and the secondaries' entry mapped there too are taken down, which
    // below the split never exist.
    Gate::new("test-boot", "boot-highmem", false)
        .only(Arch::Armv7a)
        .with_args(&["--memory", "3072"]),
    // A PC with neither an HPET nor `RDSEED`: its clock is the TSC measured
    // against the PIT, and its random words come from `RDRAND`. Both are
    // what an older machine takes, and neither runs on the default `q35`.
    Gate::new("test-boot", "boot-legacy", false)
        .only(Arch::X86_64)
        .with_env(&[
            ("FERRIX_X86_MACHINE", "hpet=off"),
            (
                "FERRIX_X86_CPU",
                "qemu64,+pdpe1gb,+smep,+smap,+umip,+rdrand",
            ),
        ]),
    // `ferrix.onexit=reset`: the reset through the FADT's register, which
    // every other gate's power-off never takes.
    Gate::new("test-boot", "boot-reset", false)
        .only(Arch::X86_64)
        .resetting(),
    // One processor: a scoped shootdown with nobody else online flushes
    // only here, which no multiprocessor boot does once discovery is done.
    Gate::new("test-boot", "boot-single", false)
        .only(Arch::X86_64)
        .with_args(&["--smp", "1"]),
    // The options a boot reads that no other gate gives it: the boot console
    // drawn on the framebuffer, which the Pixel 7 asks for; a pid 1 named on
    // the command line that the image does not have, which is said and falls
    // back to the built-in program; a `ferrix.onexit` the kernel does not
    // understand, which is said and powers off as the default does; and
    // `nokaslr`, the layout a debugger asks for, which every other boot moves.
    // ARMv7-A's `virt` has no framebuffer, so there the console finds none to
    // draw on.
    Gate::new("test-boot", "boot-options", false).with_args(&[
        "--kernel-option",
        "ferrix.fbcon",
        "--kernel-option",
        "ferrix.init=/sbin/no-such-init",
        "--kernel-option",
        "ferrix.onexit=halt",
        "--kernel-option",
        "nokaslr",
    ]),
    Gate::new("test-shell", "shell", true),
    Gate::new("test-vfs", "vfs", true),
    Gate::new("test-net", "net", true),
    Gate::new("test-threads", "threads", false),
    Gate::new("test-pty", "pty", false),
    Gate::new("test-btrfs", "btrfs", false),
    Gate::new("test-powerfail", "powerfail", false),
    Gate::new("test-display", "display", false),
    Gate::new("test-input", "input", false),
    Gate::new("test-jobs", "jobs", false).only(Arch::X86_64),
    Gate::new("test-restart", "restart", false).only(Arch::X86_64),
    Gate::new("test-sysfs", "sysfs", false).only(Arch::X86_64),
];

/// Where the floor each architecture's coverage may not fall below is kept.
const FLOOR: &str = "docs/certification/coverage-floor.json";

/// `cargo xtask coverage`: run [`SUITE`] under the plugin on each
/// architecture asked for, report the union, and fail below the recorded
/// floor.
///
/// The plugin is `FERRIX_DRCOV`, the path of QEMU's `libdrcov.so`, which
/// QEMU builds in `contrib/plugins/` and distributions do not package.
/// Traces go to `--to DIR`, or `build/coverage/<arch>`.
///
/// # Errors
///
/// No plugin, no `--init`, a gate that failed, or coverage below the floor.
pub(crate) fn run(args: &Args) -> Result<()> {
    let plugin = std::env::var("FERRIX_DRCOV").map_err(|_| {
        Error::new(
            "coverage needs FERRIX_DRCOV, the path of QEMU's drcov plugin \
             (contrib/plugins/libdrcov.so in a QEMU build tree)",
        )
    })?;
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new(
            "coverage needs --init PATH, a static busybox for each architecture; \
             `{arch}` in the path is replaced by the architecture's name",
        )
    })?;
    let mut failed = Vec::new();
    for arch in args.arches()? {
        let directory = match args.to.as_deref() {
            Some(to) => PathBuf::from(to).join(arch.name()),
            None => paths::workspace_root()
                .join("build")
                .join("coverage")
                .join(arch.name()),
        };
        clear(&directory)?;
        for gate in SUITE {
            if gate.only.is_some_and(|only| only != arch) {
                continue;
            }
            if let Err(error) = run_gate(arch, gate, &plugin, init, &directory, args) {
                eprintln!("\n  {error}");
                failed.push(format!("{} on {arch}", gate.command));
            }
        }
        if let Err(error) = report(arch, &directory) {
            eprintln!("\n  {error}");
            failed.push(format!("the coverage of {arch}"));
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "coverage: failed: {}",
            failed.join(", ")
        )))
    }
}

/// Remove what an earlier run left in `directory`: its traces, their
/// kernel and slide sidecars and the kernels they named.
fn clear(directory: &Path) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let ours = name.ends_with(".drcov")
            || name.ends_with(".drcov.kernel")
            || name.ends_with(".drcov.slide")
            || (name.starts_with("ferrix-kernel-") && name.ends_with(".elf"));
        if ours {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Run one gate as a child of this program, its boots traced into
/// `directory`.
fn run_gate(
    arch: Arch,
    gate: &Gate,
    plugin: &str,
    init: &str,
    directory: &Path,
    args: &Args,
) -> Result<()> {
    let trace = directory.join(format!("{}.drcov", gate.name));
    let program = std::env::current_exe()?;
    let mut command = Command::new(program);
    let _ = command
        .current_dir(paths::workspace_root())
        .env(
            "FERRIX_QEMU_PLUGIN",
            format!("{plugin},filename={}", trace.display()),
        )
        .args([gate.command, "--arch", arch.name(), "--accel", "tcg"]);
    // Two processors on ARMv7-A, the board's count: QEMU's default of four
    // hides the failures only two show.
    if arch == Arch::Armv7a {
        let _ = command.args(["--smp", "2"]);
    }
    if gate.userland {
        let _ = command.args(["--init", init]);
    }
    if gate.reset {
        let _ = command.arg("--reset");
    }
    if let Some(machine) = gate.arm_machine {
        let _ = command.env("FERRIX_ARM_MACHINE", machine);
    }
    if let Some(cpu) = gate.arm_cpu {
        let _ = command.env("FERRIX_ARM_CPU", cpu);
    }
    let _ = command.envs(gate.env.iter().copied()).args(gate.extra);
    if args.release {
        let _ = command.arg("--release");
    }
    if args.timeout_given {
        let _ = command.args(["--timeout", &args.timeout.to_string()]);
    }
    println!("\n  coverage: {arch}: {} ({})", gate.command, gate.name);
    crate::cargo::run(command, &format!("{} on {arch}", gate.command))
}

/// Run the report over every trace in `directory`, against the recorded
/// floor, and the decision report after it.
fn report(arch: Arch, directory: &Path) -> Result<()> {
    let mut traces = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "drcov")
        {
            traces.push(path);
        }
    }
    traces.sort();
    // A trace of a QEMU that was killed, not stopped, is empty: said, and
    // left out. `test-powerfail`'s cuts are the ones that are meant to be.
    traces.retain(|trace| {
        let empty = std::fs::metadata(trace).map_or(true, |meta| meta.len() == 0);
        if empty {
            println!(
                "  coverage: {} is empty: its QEMU was killed, not stopped",
                trace.display()
            );
        }
        !empty
    });
    let boot = directory.join("boot.drcov");
    let reference = std::fs::read_to_string(sidecar(&boot)).map_err(|error| {
        Error::new(format!(
            "{arch}: no kernel recorded for {}: {error}; test-boot has to have run",
            boot.display()
        ))
    })?;
    let elf = directory.join(reference.trim());

    let mut arguments = vec![
        "--arch".to_owned(),
        arch.name().to_owned(),
        "--floor".to_owned(),
        FLOOR.to_owned(),
        "--elf".to_owned(),
        elf.display().to_string(),
        "--drcov".to_owned(),
    ];
    arguments.extend(traces.iter().map(|trace| trace.display().to_string()));
    println!(
        "\n  coverage: {arch}: python3 tools/common/gen/coverage-report.py {}",
        arguments.join(" ")
    );
    let borrowed: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let statements = crate::check::python_with("tools/common/gen/coverage-report.py", &borrowed);

    // Decision coverage from the same traces (finding F-13): reported, with
    // no floor yet, whether or not the statements met theirs.
    let mut decisions = vec![
        "--arch".to_owned(),
        arch.name().to_owned(),
        "--elf".to_owned(),
        elf.display().to_string(),
        "--drcov".to_owned(),
    ];
    decisions.extend(traces.iter().map(|trace| trace.display().to_string()));
    println!(
        "\n  coverage: {arch}: python3 tools/common/gen/decision-coverage.py {}",
        decisions.join(" ")
    );
    let borrowed: Vec<&str> = decisions.iter().map(String::as_str).collect();
    let decided = crate::check::python_with("tools/common/gen/decision-coverage.py", &borrowed);
    statements.and(decided)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_boot_keeps_its_name_and_later_ones_are_numbered() {
        let plugin = "/q/libdrcov.so,filename=/c/shell.drcov";
        assert_eq!(
            numbered(plugin, 1),
            (plugin.to_owned(), Some(PathBuf::from("/c/shell.drcov")))
        );
        // The numbered name is joined with the host's own separator, which
        // is the one the host's QEMU reads: `\` on Windows.
        let third = Path::new("/c").join("shell.3.drcov");
        assert_eq!(
            numbered(plugin, 3),
            (
                format!("/q/libdrcov.so,filename={}", third.display()),
                Some(third)
            )
        );
    }

    #[test]
    fn a_plugin_without_a_filename_is_passed_on() {
        assert_eq!(
            numbered("/q/libdrcov.so", 2),
            ("/q/libdrcov.so".to_owned(), None)
        );
    }

    #[test]
    fn a_trace_without_an_extension_is_numbered_at_the_end() {
        assert_eq!(
            numbered_path(Path::new("/c/trace"), 2),
            PathBuf::from("/c/trace.2")
        );
    }

    #[test]
    fn the_sidecar_is_the_trace_with_kernel_added() {
        assert_eq!(
            sidecar(Path::new("/c/shell.2.drcov")),
            PathBuf::from("/c/shell.2.drcov.kernel")
        );
    }
}
