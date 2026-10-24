//! Driving `cargo` for the two freestanding crates.
//!
//! The loader and the kernel are built for different targets from the same
//! workspace, which is why neither can be a default member and why `cargo
//! build` at the root builds only this tool.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::builds::Build;
use crate::paths::{self, Arch};
use crate::{Error, Result};

/// Build the UEFI loader for `arch` and return the `.efi` firmware will run.
///
/// On the 64-bit pair that is what rustc produced. On ARMv7-A rustc produces
/// an ELF static PIE, and the `.efi` is written beside it by `pe::convert` —
/// which checks the ELF against `src/boot/common/uefi/linker/armv7a.ld`'s contract, so a
/// loader that breaks it fails the build rather than the boot.
pub(crate) fn build_loader(arch: Arch, release: bool) -> Result<PathBuf> {
    let name = if arch.loader_is_elf() {
        "ferrix-boot"
    } else {
        "ferrix-boot.efi"
    };
    let made = output(arch.loader_target(), release, name);
    build("ferrix-boot", arch.loader_target(), release)?
        .output(&made)
        .run()?;
    let made = artifact(made)?;
    let loader = std::fs::read(&made)
        .map_err(|error| Error::new(format!("reading {}: {error}", made.display())))?;
    crate::signing::refuse_development_anchors("the UEFI loader", &loader)?;
    if !arch.loader_is_elf() {
        return Ok(made);
    }

    let elf = made;
    let bytes = std::fs::read(&elf)
        .map_err(|error| Error::new(format!("reading {}: {error}", elf.display())))?;
    crate::pe::check_switch(&bytes)?;
    let image = crate::pe::convert(&bytes)?;
    let efi = elf.with_extension("efi");
    std::fs::write(&efi, &image)?;
    println!("  converted the loader to PE32, {} KiB", image.len() / 1024);
    Ok(efi)
}

/// Whether this run builds its kernels with `mitigations=off`, for a check
/// that has to know what the image it boots was built to do. False when the
/// run's flags do not resolve, which no kernel was built from.
pub(crate) fn mitigations_off() -> bool {
    crate::flags::this_run().is_ok_and(crate::flags::Resolved::mitigations_off)
}

/// Whether this run's release kernels are built in the `iterate` profile.
static ITERATE: AtomicBool = AtomicBool::new(false);

/// Say how every kernel this run builds is to be built: `main` calls it once,
/// with `--kernel-profile` and the `--set`s ([`crate::flags`]), and whether
/// `--iterate` asked for a release kernel linked for working on it.
///
/// # Errors
///
/// `--iterate` without `--release`, which would change nothing.
pub(crate) fn set_kernel_build(
    profile: Option<String>,
    sets: Vec<(String, String)>,
    iterate: bool,
    release: bool,
) -> Result<()> {
    if iterate && !release {
        return Err(Error::new(
            "--iterate changes how a --release kernel is linked; give --release with it",
        ));
    }
    crate::flags::ask(profile, sets);
    ITERATE.store(iterate, Ordering::Relaxed);
    Ok(())
}

/// Whether this run's debug kernels carry the full DWARF: `--gdb`.
static FULL_DEBUG: AtomicBool = AtomicBool::new(false);

/// Say whether every debug kernel this run builds carries the full DWARF,
/// locals and types for a debugger, rather than the dev profile's line tables
/// (`Cargo.toml`): `main` calls it once, with whether `--gdb` was given. It
/// is a `--config` of its own, so cargo keeps both builds side by side.
pub(crate) fn set_full_debug(full: bool) {
    FULL_DEBUG.store(full, Ordering::Relaxed);
}

/// The `--config` that gives a kernel built in `profile` the full DWARF, when
/// `full` ([`FULL_DEBUG`]) asks for it: only a debug kernel's, since the
/// release profiles keep their own `debug` setting.
fn full_debug_config(profile: &str, full: bool) -> Option<&'static str> {
    (profile == "debug" && full).then_some("profile.dev.debug=true")
}

/// The environment the kernel's build script reads its flags from
/// (`src/kernel/build.rs`): the resolved `cfg`s, and every declared flag
/// and value for `--check-cfg`.
pub(crate) fn flag_environment(resolved: &crate::flags::Resolved) -> [(&'static str, String); 2] {
    [
        ("FERRIX_BUILD_FLAGS", resolved.cfg_words()),
        ("FERRIX_FLAG_VALUES", resolved.declared_words()),
    ]
}

/// The cargo profile a kernel is built in, and so the directory cargo puts
/// it in: `iterate` only for a release kernel with `--iterate`, and
/// `coverage` for a debug kernel whose boots a TCG plugin traces.
fn kernel_profile(release: bool) -> &'static str {
    profile_for(
        release,
        ITERATE.load(Ordering::Relaxed),
        std::env::var_os("FERRIX_QEMU_PLUGIN").is_some(),
    )
}

/// [`kernel_profile`], from what it reads.
///
/// `traced`: `FERRIX_QEMU_PLUGIN` is set, which `cargo xtask coverage` sets
/// for every gate it runs. The dev profile builds without LTO, for a fast
/// edit; the `coverage` profile is the dev profile with rustc's crate-local
/// thin LTO kept, the code every debug coverage figure was measured on
/// (`Cargo.toml`; the certification consultant's ruling, ledger line 1472,
/// route B). Taking it from the plugin rather than from an option means no
/// traced boot can measure the other build. A release kernel is traced as
/// it is.
fn profile_for(release: bool, iterate: bool, traced: bool) -> &'static str {
    match (release, iterate, traced) {
        (false, _, false) => "debug",
        (false, _, true) => "coverage",
        (true, false, _) => "release",
        (true, true, _) => "iterate",
    }
}

/// The `--config` that builds every crate for `target` for a kernel without
/// its side-channel defences, and without KASLR: the static relocation
/// model, which on x86-64 is the code the kernel was built as before it
/// moved, and which `src/kernel/build.rs` links at its fixed address when the
/// flags say `mitigations=off`.
///
/// A `--config` array is *appended* to the one in `.cargo/config.toml`, so the
/// per-target flags [`refuse_inherited_rustflags`] protects are kept and the
/// model is added -- where `RUSTFLAGS` would have replaced them. It is
/// codegen, so it reaches every crate built for the target; the flag itself
/// reaches the kernel crate alone ([`crate::flags`]).
pub(crate) fn mitigations_off_config(target: &str) -> String {
    format!("target.{target}.rustflags=[\"-C\",\"relocation-model=static\"]")
}

/// Where a kernel without its defences is built: a target directory of its
/// own, so that building one setting does not throw away the other's cache.
pub(crate) fn mitigations_off_target_dir() -> PathBuf {
    paths::target_dir().join("mitigations-off")
}

/// Where the kernel ELF for `arch` is once built with `release` as asked and
/// the default flags: what `xtask symbolize` names addresses from.
pub(crate) fn kernel_elf(arch: Arch, release: bool) -> PathBuf {
    paths::target_dir()
        .join(arch.kernel_target())
        .join(kernel_profile(release))
        .join("ferrix-kernel")
}

/// `cargo build -p ferrix-kernel` for `arch`, with the setting
/// [`set_kernel_build`] chose, and where the ELF it makes will be.
fn kernel(arch: Arch, release: bool) -> Result<(Build, PathBuf)> {
    let target = arch.kernel_target();
    let profile = kernel_profile(release);
    let flags = crate::flags::this_run()?;
    let mut build = match profile {
        "iterate" => {
            println!("  with --iterate: thin LTO, not the release kernel a gate builds");
            build("ferrix-kernel", target, false)?.args(["--profile", "iterate"])
        }
        "coverage" => {
            println!(
                "  traced: the coverage profile, with the crate-local ThinLTO of the measured build"
            );
            build("ferrix-kernel", target, false)?.args(["--profile", "coverage"])
        }
        _ => build("ferrix-kernel", target, release)?,
    };
    if let Some(config) = full_debug_config(profile, FULL_DEBUG.load(Ordering::Relaxed)) {
        println!("  with --gdb: the full DWARF");
        build = build.args(["--config", config]);
    }
    for (key, value) in flag_environment(flags) {
        build = build.env(key, value);
    }
    if flags.profile == "none" {
        println!(
            "  an ad-hoc build of profile {}, no claim: {}",
            flags.base,
            flags
                .changed
                .iter()
                .map(|(flag, value)| format!("{flag}={value}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    } else if flags.profile != crate::flags::DEFAULT_PROFILE {
        println!("  profile {} (level {})", flags.profile, flags.level);
    }
    if !flags.mitigations_off() {
        let made = paths::target_dir()
            .join(target)
            .join(profile)
            .join("ferrix-kernel");
        return Ok((build, made));
    }
    println!("  with --mitigations off: no side-channel defences");
    let directory = mitigations_off_target_dir();
    let made = directory.join(target).join(profile).join("ferrix-kernel");
    let build = build
        .args(["--config", &mitigations_off_config(target)])
        .args([std::ffi::OsStr::new("--target-dir"), directory.as_os_str()]);
    Ok((build, made))
}

/// A built kernel, and what the initramfs of an image of it must carry for
/// pid 1 under `.ferrix/init/` (`src/kernel/src/init.rs`): a program, a
/// script for its `sh -c`, or a list of commands. Until 2026-10-04 these were
/// compiled into the kernel, so that each test's init was a kernel of its
/// own; now one kernel serves every test, and an image is written from both
/// together (`crate::fat`, `crate::flash`), so an init asked for cannot be
/// left out of it. Dereferences to the ELF, for what only reads that.
#[derive(Debug, Clone)]
pub(crate) struct Kernel {
    /// The ELF.
    pub(crate) elf: PathBuf,
    /// The program pid 1 runs, when one was asked for.
    program: Option<PathBuf>,
    /// The script it runs with `sh -c`, empty for an interactive shell.
    script: String,
    /// A list `vfs::encode` wrote, run in place of the program.
    commands: Option<PathBuf>,
}

impl Kernel {
    /// A kernel whose images carry no init of their own.
    pub(crate) fn plain(elf: PathBuf) -> Kernel {
        Kernel {
            elf,
            program: None,
            script: String::new(),
            commands: None,
        }
    }

    /// What pid 1's inputs are, by name under `.ferrix/init/`.
    fn inputs(&self) -> Result<Vec<(&'static str, Vec<u8>)>> {
        let read = |path: &Path| {
            std::fs::read(path)
                .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
        };
        let mut inputs = Vec::new();
        if let Some(program) = &self.program {
            inputs.push(("program", read(program)?));
        }
        if !self.script.is_empty() {
            inputs.push(("script", self.script.clone().into_bytes()));
        }
        if let Some(commands) = &self.commands {
            inputs.push(("commands", read(commands)?));
        }
        Ok(inputs)
    }

    /// `archive` with this kernel's init inputs added, read back to be sure
    /// the image carries what was asked for: a test whose init went missing
    /// would boot `/sbin/init` or nothing and say little about why.
    ///
    /// # Errors
    ///
    /// An input that cannot be read, an archive that does not parse, or one
    /// that does not carry an input after it was added.
    pub(crate) fn initramfs(&self, archive: &[u8]) -> Result<Vec<u8>> {
        let inputs = self.inputs()?;
        let borrowed: Vec<(&str, &[u8])> = inputs
            .iter()
            .map(|(name, data)| (*name, data.as_slice()))
            .collect();
        let built = crate::initramfs::with_init_inputs(archive, &borrowed)?;
        carries(&built, &inputs)?;
        Ok(built)
    }
}

impl std::ops::Deref for Kernel {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.elf
    }
}

/// Whether `archive` carries each of `inputs` under `.ferrix/init/`, byte
/// for byte.
fn carries(archive: &[u8], inputs: &[(&'static str, Vec<u8>)]) -> Result<()> {
    let carried = crate::initramfs::init_inputs(archive)?;
    for (name, data) in inputs {
        if !carried
            .iter()
            .any(|(found, bytes)| found == name && bytes == data)
        {
            return Err(Error::new(format!(
                "the initramfs does not carry pid 1's {name}, which was asked for"
            )));
        }
    }
    Ok(())
}

/// Build the kernel for `arch` and return it, with no init of its own.
pub(crate) fn build_kernel(arch: Arch, release: bool) -> Result<Kernel> {
    let (build, made) = kernel(arch, release)?;
    build.output(&made).run()?;
    let made = artifact(made)?;
    crate::signing::refuse_verifier_in_kernel_file(&made)?;
    Ok(Kernel::plain(made))
}

/// Compile, before images for several architectures are made one after
/// another, what each of them builds: every architecture's kernel in one
/// cargo call, then the loaders in one, then each native program in one.
///
/// cargo takes a lock on the target directory, so the per-architecture
/// calls that follow can never overlap; but one call that names several
/// `--target`s compiles them side by side. A release kernel is one rustc
/// that links with fat LTO over one codegen unit, about two minutes on one
/// thread, and three of them one after another were 360 s of a cold
/// `build --arch all --release`'s 443 s (`docs/TEST-TIME.md`, B1). Each
/// call names one package, so no feature is unified across packages that
/// are built apart below (the drivers enable different features of
/// `ferrix-driver`), and what each target gets is what its own call would
/// have built: the calls that follow find it fresh and make the images from
/// the same files.
///
/// Skipped while `FERRIX_BUILDS` records or replays, whose plan is the
/// per-architecture builds alone.
///
/// # Errors
///
/// A cargo call that fails.
pub(crate) fn build_ahead(arches: &[Arch], release: bool) -> Result<()> {
    if arches.len() < 2 || crate::builds::active() {
        return Ok(());
    }
    refuse_inherited_rustflags()?;
    let kernels: Vec<&str> = arches.iter().map(|arch| arch.kernel_target()).collect();
    let loaders: Vec<&str> = arches.iter().map(|arch| arch.loader_target()).collect();

    let flags = crate::flags::this_run()?;
    let mut kernel = ahead("ferrix-kernel", &kernels);
    let _ = kernel.envs(flag_environment(flags));
    match kernel_profile(release) {
        "iterate" => {
            let _ = kernel.args(["--profile", "iterate"]);
        }
        "coverage" => {
            let _ = kernel.args(["--profile", "coverage"]);
        }
        "release" => {
            let _ = kernel.arg("--release");
        }
        profile => {
            if let Some(config) = full_debug_config(profile, FULL_DEBUG.load(Ordering::Relaxed)) {
                let _ = kernel.args(["--config", config]);
            }
        }
    }
    if flags.mitigations_off() {
        for target in &kernels {
            let _ = kernel.args(["--config", &mitigations_off_config(target)]);
        }
        let _ = kernel.arg("--target-dir").arg(mitigations_off_target_dir());
    }
    run(
        kernel,
        "cargo build -p ferrix-kernel, every architecture at once",
    )?;

    let packages = std::iter::once("ferrix-boot")
        .map(|package| (package, &loaders))
        .chain(
            crate::native::PROGRAMS
                .iter()
                .map(|program| (program.package, &kernels)),
        );
    for (package, targets) in packages {
        let mut command = ahead(package, targets);
        if release {
            let _ = command.arg("--release");
        }
        run(
            command,
            &format!("cargo build -p {package}, every architecture at once"),
        )?;
    }
    Ok(())
}

/// `cargo build -p <package>` for each of `targets`, in one call.
fn ahead(package: &str, targets: &[&str]) -> Command {
    println!("  building {package} for {}, at once", targets.join(", "));
    let mut command = Command::new(cargo());
    let _ = command
        .current_dir(paths::workspace_root())
        .args(["build", "--package", package]);
    for target in targets {
        let _ = command.args(["--target", target]);
    }
    command
}

/// Build `binary`, a native program in `package`, for `arch` into
/// `target_dir`, and return the ELF.
///
/// For the kernel's own target, which is what the kernel's ELF loader takes:
/// freestanding, soft float, and statically relocated by `.cargo/config.toml`.
/// The target directory is a parameter so that a test can build into one of
/// its own and never wait on the lock of the build running it.
pub(crate) fn build_native(
    arch: Arch,
    release: bool,
    package: &str,
    binary: &str,
    target_dir: &Path,
) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let path = target_dir
        .join(arch.kernel_target())
        .join(profile)
        .join(binary);
    build(package, arch.kernel_target(), release)?
        .env("CARGO_TARGET_DIR", target_dir)
        .output(&path)
        .run()?;
    artifact(path)
}

/// [`build_native`] for every architecture in one cargo call, which
/// compiles the targets side by side, as [`build_ahead`] does: the ELFs,
/// by architecture. One package and the same features for each target, so
/// each is what its own call would have built. Not a [`Build`]: for the
/// tests, which `FERRIX_BUILDS` never records.
#[cfg(test)]
pub(crate) fn build_native_every_arch(
    release: bool,
    package: &str,
    binary: &str,
    target_dir: &Path,
) -> Result<Vec<(Arch, PathBuf)>> {
    refuse_inherited_rustflags()?;
    let targets: Vec<&str> = Arch::ALL.iter().map(|arch| arch.kernel_target()).collect();
    let mut command = ahead(package, &targets);
    let _ = command.env("CARGO_TARGET_DIR", target_dir);
    if release {
        let _ = command.arg("--release");
    }
    run(
        command,
        &format!("cargo build -p {package}, every architecture at once"),
    )?;
    let profile = if release { "release" } else { "debug" };
    Arch::ALL
        .iter()
        .map(|arch| {
            let path = target_dir
                .join(arch.kernel_target())
                .join(profile)
                .join(binary);
            artifact(path).map(|path| (*arch, path))
        })
        .collect()
}

/// [`build_native`], with `variables` set in the build's environment: a
/// variant of a program some `option_env!` of its own turns on, into a target
/// directory of its own so the plain build is not rebuilt on the way back.
pub(crate) fn build_native_with(
    arch: Arch,
    release: bool,
    package: &str,
    binary: &str,
    target_dir: &Path,
    variables: &[(&str, &str)],
) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let path = target_dir
        .join(arch.kernel_target())
        .join(profile)
        .join(binary);
    let mut build =
        build(package, arch.kernel_target(), release)?.env("CARGO_TARGET_DIR", target_dir);
    for (name, value) in variables {
        build = build.env(name, value);
    }
    build.output(&path).run()?;
    artifact(path)
}

/// [`build_native`], with `FERRIX_DRIVER_VERSION` set to `version`: a
/// driver that says that version once at its start, for
/// `cargo xtask test-restart --update` to tell from the initramfs's
/// (`docs/DEVMGR.md` §4.1).
pub(crate) fn build_native_version(
    arch: Arch,
    release: bool,
    package: &str,
    binary: &str,
    target_dir: &Path,
    version: &str,
) -> Result<PathBuf> {
    let profile = if release { "release" } else { "debug" };
    let path = target_dir
        .join(arch.kernel_target())
        .join(profile)
        .join(binary);
    build(package, arch.kernel_target(), release)?
        .env("CARGO_TARGET_DIR", target_dir)
        .env("FERRIX_DRIVER_VERSION", version)
        .output(&path)
        .run()?;
    artifact(path)
}

/// The kernel, with `init` as pid 1's program, told to run `script` with
/// `sh -c`: both go in the image's initramfs, not in the kernel ([`Kernel`]).
///
/// Taken from the arguments rather than this process's environment, so the
/// command a person typed is the whole of what was built: a `FERRIX_INIT` left
/// exported in the shell cannot quietly substitute a different program.
pub(crate) fn build_kernel_with_init(
    arch: Arch,
    release: bool,
    init: &Path,
    script: &str,
) -> Result<Kernel> {
    if script.contains('\0') {
        return Err(Error::new(
            "the init script contains a NUL, which cannot survive being an argument",
        ));
    }
    let mut kernel = build_kernel(arch, release)?;
    kernel.program = Some(init.to_path_buf());
    kernel.script = script.to_owned();
    Ok(kernel)
}

/// The kernel, told to run the commands in `commands`, a file
/// `vfs::encode` wrote, in place of a shell: the list goes in the image's
/// initramfs ([`Kernel`]).
pub(crate) fn build_kernel_with_commands(
    arch: Arch,
    release: bool,
    commands: &Path,
) -> Result<Kernel> {
    let list = std::fs::read(commands)
        .map_err(|error| Error::new(format!("reading {}: {error}", commands.display())))?;
    // Refused here so that a list cut short fails the build rather than
    // losing its last command at boot, where init refuses it too.
    if !list.is_empty() && !list.ends_with(b"\0\0") {
        return Err(Error::new(format!(
            "{} does not end its last command with an empty argument",
            commands.display()
        )));
    }
    let mut kernel = build_kernel(arch, release)?;
    kernel.commands = Some(commands.to_path_buf());
    Ok(kernel)
}

/// `cargo build -p <package> --target <target>`, for the caller to add to
/// and run: a [`Build`], which `FERRIX_BUILDS` may record or replay.
fn build(package: &str, target: &str, release: bool) -> Result<Build> {
    refuse_inherited_rustflags()?;
    println!("  building {package} for {target}");
    let build = Build::cargo(
        format!("cargo build -p {package} --target {target}"),
        paths::workspace_root(),
    )
    .args(["build", "--package", package, "--target", target]);
    Ok(if release {
        build.args(["--release"])
    } else {
        build
    })
}

/// Refuse to build with `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` set.
///
/// Either one *replaces* the per-target `rustflags` in `.cargo/config.toml`
/// rather than adding to them, and those flags are what put the kernel where the
/// loader maps it: the linker script, the page size, the static relocation
/// model. Without them the build still succeeds and the image is still written,
/// and the failure arrives at boot as a loader panic that names neither the
/// variable nor the cause. CI set `RUSTFLAGS: -D warnings` for its whole
/// workflow and lost its first boot test exactly that way.
fn refuse_inherited_rustflags() -> Result<()> {
    for variable in ["RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS"] {
        if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            return Err(Error::new(format!(
                "{variable} is set. {}",
                concat!(
                    "It replaces the per-target rustflags in .cargo/config.toml rather ",
                    "than adding to them, which drops the kernel's linker script: the ",
                    "image would build and then fail to boot. Unset it, and deny ",
                    "warnings with `cargo clippy -- -D warnings` instead.",
                ),
            )));
        }
    }
    Ok(())
}

/// Where cargo writes `name` for `target` in the target directory.
fn output(target: &str, release: bool, name: &str) -> PathBuf {
    let profile = if release { "release" } else { "debug" };
    paths::target_dir().join(target).join(profile).join(name)
}

/// `path`, checked for existence so that a rename in a manifest fails here
/// rather than as a confusing image error.
fn artifact(path: PathBuf) -> Result<PathBuf> {
    if !path.is_file() {
        return Err(Error::new(format!(
            "cargo reported success but {} does not exist",
            path.display()
        )));
    }
    Ok(path)
}

/// Run a command, turning a non-zero status into an error that names it.
pub(crate) fn run(mut command: Command, description: &str) -> Result<()> {
    let status = command
        .status()
        .map_err(|error| Error::new(format!("could not run {description}: {error}")))?;

    finished(status, description)
}

/// Turn a finished command's `status` into an error that names it, or into
/// nothing at all.
///
/// Apart from [`run`], which waits itself, a caller that had to spawn the
/// command to get at its output — `noise::run` — ends up here, so that a
/// failure reads the same whichever of them started it.
pub(crate) fn finished(status: std::process::ExitStatus, description: &str) -> Result<()> {
    if !status.success() {
        return Err(Error::new(format!(
            "{description} failed{}",
            status
                .code()
                .map_or(String::new(), |code| format!(" (exit {code})"))
        )));
    }
    Ok(())
}

/// The cargo to re-enter with, so that a `+toolchain` invocation stays on the
/// toolchain the user chose.
pub(crate) fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_traced_debug_kernel_is_the_coverage_build() {
        assert_eq!(profile_for(false, false, false), "debug");
        assert_eq!(profile_for(false, false, true), "coverage");
        assert_eq!(profile_for(true, false, false), "release");
        assert_eq!(profile_for(true, false, true), "release", "traced as built");
        assert_eq!(profile_for(true, true, true), "iterate");
    }

    #[test]
    fn only_a_debug_kernel_under_gdb_gets_the_full_dwarf() {
        assert_eq!(
            full_debug_config("debug", true),
            Some("profile.dev.debug=true")
        );
        assert_eq!(full_debug_config("debug", false), None, "line tables");
        for profile in ["release", "iterate"] {
            assert_eq!(full_debug_config(profile, true), None, "{profile}");
            assert_eq!(full_debug_config(profile, false), None, "{profile}");
        }
    }

    fn kernel_with(program: Option<&[u8]>, script: &str) -> (Kernel, tempfile_dir::Dir) {
        let dir = tempfile_dir::Dir::new();
        let mut kernel = Kernel::plain(dir.path().join("ferrix-kernel"));
        if let Some(bytes) = program {
            let path = dir.path().join("program");
            std::fs::write(&path, bytes).unwrap();
            kernel.program = Some(path);
        }
        kernel.script = script.to_owned();
        (kernel, dir)
    }

    /// A directory of its own under the target directory, removed when dropped.
    mod tempfile_dir {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT: AtomicUsize = AtomicUsize::new(0);

        pub(super) struct Dir(PathBuf);

        impl Dir {
            pub(super) fn new() -> Dir {
                let path = std::env::temp_dir().join(format!(
                    "xtask-cargo-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }

            pub(super) fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn an_images_initramfs_carries_pid_1s_program_and_script() {
        let (kernel, _dir) = kernel_with(Some(b"\x7fELF busybox"), "echo hi");
        let base = crate::initramfs::plain(&["bin"], &[("bin/true", 0o755, b"t")]).unwrap();
        let built = kernel.initramfs(&base).unwrap();
        let carried = crate::initramfs::init_inputs(&built).unwrap();
        assert_eq!(
            carried,
            vec![
                ("program".to_owned(), b"\x7fELF busybox".to_vec()),
                ("script".to_owned(), b"echo hi".to_vec()),
            ]
        );
    }

    #[test]
    fn a_plain_kernels_initramfs_is_the_archive_it_was_given_with_nothing_added() {
        let (kernel, _dir) = kernel_with(None, "");
        let base = crate::initramfs::plain(&["bin"], &[("bin/true", 0o755, b"t")]).unwrap();
        let built = kernel.initramfs(&base).unwrap();
        assert!(crate::initramfs::init_inputs(&built).unwrap().is_empty());
        assert_eq!(built, base);
    }

    #[test]
    fn an_init_asked_for_and_not_carried_is_an_error_not_a_quiet_default() {
        let base = crate::initramfs::plain(&["bin"], &[("bin/true", 0o755, b"t")]).unwrap();
        let asked = [("program", b"\x7fELF".to_vec())];
        let error = carries(&base, &asked).unwrap_err().to_string();
        assert!(error.contains("does not carry pid 1's program"), "{error}");
        // Carried with other bytes is not carried either.
        let other = crate::initramfs::with_init_inputs(&base, &[("program", b"else")]).unwrap();
        assert!(carries(&other, &asked).is_err());
    }

    #[test]
    fn an_archive_that_carries_the_inputs_directory_already_is_refused() {
        let base = crate::initramfs::plain(&[".ferrix"], &[]).unwrap();
        assert!(crate::initramfs::with_init_inputs(&base, &[("script", b"x")]).is_err());
    }
}
