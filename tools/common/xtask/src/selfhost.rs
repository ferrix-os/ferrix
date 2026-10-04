//! `test-selfhost`: stage 20's first exit. Ferrix's x86-64 image is built on
//! Ferrix, by the same `cargo xtask build` a person runs on a Linux host, and
//! the image it made boots.
//!
//! # The volume
//!
//! One btrfs volume per run, made here and attached writable. It has no
//! `ferrix-root` label, so the kernel mounts it at `/data`. It holds:
//!
//! * the toolchain tree `tools/common/fetch/fetch-rustc-sysroot.sh` keeps beside its own
//!   image: rustc and Cargo, the standard libraries for the host,
//!   `x86_64-unknown-none` and `x86_64-unknown-uefi`, and Debian's glibc and
//!   gcc driver, copied into the staging directory (extents shared where the
//!   host can);
//! * `src/`, every file git tracks in this checkout and in each component's
//!   checkout (`components.toml`), as it is in the work tree, so an
//!   uncommitted change is built there as it would be here;
//! * `vendor/`, the workspace's crates.io dependencies from `cargo vendor
//!   --offline`, and a Cargo home whose configuration points at them, since
//!   nothing in the guest's build goes to the network;
//! * [`ROOM`] for the target directory and the image, sparse on the host.
//!
//! The initramfs is zinc and [`rustc::LINKS`], as `test-rustc`'s is: the
//! absolute paths glibc and gcc name, into the volume.
//!
//! # The build
//!
//! zinc runs [`script`]: `cargo xtask build --arch x86_64`, which compiles
//! xtask for the guest, a glibc program like rustc; xtask then has Cargo
//! compile the loader, the kernel and the native programs, and writes the FAT
//! image, as it does on any host. When the shell exits the guest powers off,
//! and the kernel commits `/data` on the way, so this waits for QEMU to exit
//! by itself rather than killing it ([`qemu::watch_to_power_off`]).
//!
//! # The judgement
//!
//! The script's own status and the line xtask prints for the image it wrote;
//! then `btrfs check` over the volume, since Ferrix wrote a gigabyte to it;
//! then `btrfs restore` takes the image and the kernel ELF out, and the image
//! must pass `test-boot`'s boot test, the kernel resolving its own
//! backtraces. It is not compared with an image built here: the paths
//! compiled into it -- `/data/src`, and the vendored crates' -- differ, and
//! so would the bytes.
//!
//! # Every build, with `--plan`
//!
//! Given `--plan <DIR>` -- a plan `FERRIX_BUILDS=record:<DIR>` wrote while
//! the test matrix ran (`crate::builds`) -- the volume carries the plan too,
//! and the crates of every workspace a build in it may compile, and zinc runs
//! `cargo xtask builds-execute --plan /data/plan` in place of `build`: Ferrix
//! makes every build the matrix made. The host then takes `/data/plan/store`
//! off the volume into `<DIR>/store`, where `FERRIX_BUILDS=replay:<DIR>/store`
//! finds it, so that the matrix boots what Ferrix compiled.
//!
//! # Where it runs
//!
//! On a Linux host: the volume is made with `mkfs.btrfs --rootdir` and read
//! back with `btrfs restore`, and the tree it starts from is made by a script
//! that needs `dpkg-deb`. x86-64 only, because the tree holds x86-64
//! binaries.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, btrfs_check, cargo, fat, initramfs, native, qemu, rustc, zinc};

/// Where the guest's Cargo home, target directory and source are, on the
/// volume.
const CARGO_HOME: &str = "/data/cargo-home";
const TARGET: &str = "/data/target";
const SOURCE: &str = "/data/src";

/// Where the vendored crates are, on the volume.
const VENDOR: &str = "/data/vendor";

/// What the guest's Cargo configuration adds to the sources `cargo vendor`
/// says to use: Cargo is told it has no network, so a crate missing from the
/// directory fails the build by name rather than as a DNS error.
const OFFLINE: &str = "\n[net]\noffline = true\n";

/// What a plan's image links beside [`rustc::LINKS`], for the script builds:
/// a POSIX `sh` and `bash` where scripts and make look for them, `env` for
/// `#!/usr/bin/env`, the kernel's UAPI headers under `/usr/include`, and the
/// magic database `file` reads.
const PLAN_LINKS: &[(&str, &str)] = &[
    ("bin/sh", "/data/usr/bin/dash"),
    ("bin/bash", "/data/usr/bin/bash"),
    ("usr/bin/env", "/data/usr/bin/env"),
    ("usr/include", "/data/usr/include"),
    ("usr/share/misc", "/data/usr/share/misc"),
    // What foot's build names absolutely: meson's `#!/usr/bin/python3`, the
    // `m4` and skeletons bison was built to find, pkg-config's own search
    // path for `wayland-scanner.pc`, and the scanner and protocol files that
    // file names under `/usr`.
    ("usr/bin/python3", "/data/usr/bin/python3"),
    ("usr/bin/m4", "/data/usr/bin/m4"),
    ("usr/share/bison", "/data/usr/share/bison"),
    ("usr/share/pkgconfig", "/data/usr/share/pkgconfig"),
    ("usr/bin/wayland-scanner", "/data/usr/bin/wayland-scanner"),
    ("usr/share/wayland", "/data/usr/share/wayland"),
];

/// Space on the volume beyond what the staging directory holds. A debug
/// build's target directory is about 1 GiB and the image 512 MiB; btrfs
/// keeps its metadata twice.
const ROOM: u64 = 8 << 30;

/// [`ROOM`] for a plan: a target directory per workspace, one per flavour of
/// the compositor's programs, and every build's outputs kept in the store.
/// Sparse on the host, which pays only for what the guest writes.
const PLAN_ROOM: u64 = 64 << 30;

/// [`MEMORY`] for a plan: every page the builds write stays in memory.
const PLAN_MEMORY: u32 = 24 * 1024;

/// [`TIMEOUT`] for a plan.
const PLAN_TIMEOUT: u64 = 6 * 3600;

/// Where the plan is on the volume.
const PLAN: &str = "/data/plan";

/// The workspaces beside the root one whose crates a build may compile.
const WORKSPACES: &[&str] = &["compositor", "zinc", "threads-test", "ferrousli"];

/// Guest memory unless `--memory` says otherwise. btrfs file pages stay in
/// memory once read or written, and the build reads the toolchain's 350 MiB
/// of libraries and writes about 1.3 GiB.
const MEMORY: u32 = 8192;

/// Seconds to wait unless `--timeout` says otherwise, for the build boot.
const TIMEOUT: u64 = 3600;

/// The status the script exits with when the build succeeded.
const STATUS: i32 = 20;

/// What the guest's xtask prints when every build of a plan was made.
const EXECUTED: &str = "builds: all ";

/// What the guest's xtask prints when it has written the image.
const BUILT: &str = "built /data/src/build/x86_64/ferrix.img";

/// Build Ferrix's x86-64 image on Ferrix, take it out of the volume, and
/// boot it.
///
/// # Errors
///
/// When the host cannot make the volume, the build boot fails or its script
/// does not finish, the volume is not a clean btrfs, or the image the guest
/// built does not pass the boot test.
pub(crate) fn test_selfhost(args: &Args) -> Result<()> {
    if let Some(volume) = &args.volume {
        return judge_volume(Path::new(volume), args);
    }
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-selfhost runs on x86-64 only: the toolchain tree holds x86-64 binaries",
        ));
    }
    if !cfg!(target_os = "linux") {
        return Err(Error::new(
            "test-selfhost runs on a Linux host: it makes its volume with mkfs.btrfs and reads \
             it back with btrfs restore",
        ));
    }
    let checker = btrfs_check::Checker::required()?;
    let tree = rustc::tree()?;
    let plan = args.plan.as_deref().map(Path::new);
    let work = paths::build_dir(arch).join("selfhost");
    let volume = stage(&tree, &work, plan)?;

    let mut build = args.clone();
    build.data_image = Some(volume.clone());
    build.data_image_kept = true;
    if !build.memory_given {
        build.memory = if plan.is_some() { PLAN_MEMORY } else { MEMORY };
    }
    if !build.timeout_given {
        build.timeout = if plan.is_some() {
            PLAN_TIMEOUT
        } else {
            TIMEOUT
        };
    }

    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose shell builds Ferrix");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(
        arch,
        args.release,
        &shell,
        &script(args.release, plan.is_some()),
    )?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    // With a plan, zinc is only the kernel's own init: in `/bin` it would be
    // `sh`, and the script builds want a POSIX one.
    let archive = if plan.is_some() {
        let mut links = rustc::LINKS.to_vec();
        links.extend_from_slice(PLAN_LINKS);
        initramfs::build(None, &natives, None, &rustc::files(&links))?
    } else {
        initramfs::build(None, &natives, Some(&bytes), &rustc::files(rustc::LINKS))?
    };
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: building Ferrix on Ferrix with {} MiB and {} processors (timeout {}s)",
        build.memory, build.smp, build.timeout
    );
    let lines = qemu::watch_to_power_off(arch, &image, &kernel, &build, crate::shell::EXITED);
    // The boot test below writes the serial log again.
    let log = paths::build_dir(arch).join("serial.log");
    let kept = work.join("build-serial.log");
    let _ = std::fs::copy(&log, &kept);
    judge(arch, &lines?, plan.is_some())?;
    println!(
        "  {arch}: the build boot's serial output is in {}",
        kept.display()
    );

    checker.run(&volume, arch)?;
    if let Some(plan) = plan {
        let store = restore_store(&volume, plan)?;
        println!(
            "  {arch}: Ferrix made every build in the plan; FERRIX_BUILDS=replay:{} answers them",
            store.display()
        );
        return Ok(());
    }
    let (built, built_kernel) = restore(arch, &volume, &work, args.release)?;
    println!("  {arch}: booting the image Ferrix built");
    qemu::test_boot(arch, &built, &built_kernel, args)?;
    println!("  {arch}: Ferrix built its own image, and it booted");
    Ok(())
}

/// The script zinc runs: Cargo's version first, so a failure says whether
/// the toolchain started at all; then the image, or with a plan every build
/// in it.
fn script(release: bool, plan: bool) -> String {
    let profile = if release { " --release" } else { "" };
    let work = if plan {
        format!("cargo xtask builds-execute --plan {PLAN}")
    } else {
        format!("cargo xtask build --arch x86_64{profile}")
    };
    format!(
        "export PATH=/data/rust/bin:/data/usr/bin:/bin\n\
         export HOME=/data/home CARGO_HOME={CARGO_HOME} CARGO_TARGET_DIR={TARGET}\n\
         cd {SOURCE}\n\
         cargo -V || exit 3\n\
         {work} || exit 4\n\
         exit {STATUS}\n"
    )
}

/// Whether the transcript is a build that wrote its image and a script that
/// got to its end.
fn judge(arch: Arch, lines: &[String], plan: bool) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at + 1..))
        .unwrap_or_default();
    let exited = after_boot
        .iter()
        .find_map(|line| line.trim().strip_prefix(crate::shell::EXITED))
        .map(str::trim);
    let built = after_boot.iter().any(|line| {
        if plan {
            line.trim().starts_with(EXECUTED)
        } else {
            line.trim_end() == BUILT
        }
    });
    match exited {
        Some(status) if status == STATUS.to_string() && built => {
            if plan {
                println!("  {arch}: every build in the plan was made on Ferrix");
            } else {
                println!("  {arch}: cargo xtask build finished on Ferrix");
            }
            Ok(())
        }
        Some("3") => Err(Error::new(format!("{arch}: `cargo -V` failed"))),
        Some("4") if plan => Err(Error::new(format!(
            "{arch}: Cargo ran, and a build in the plan failed: the build boot's serial output \
             names it, on a `builds: ... failed` line"
        ))),
        Some("4") => Err(Error::new(format!(
            "{arch}: Cargo ran, and `cargo xtask build` failed"
        ))),
        Some(status) => Err(Error::new(format!(
            "{arch}: the script exited with {status}; image line {built}"
        ))),
        None => Err(Error::new(format!("{arch}: the shell never exited"))),
    }
}

/// Make the volume in `work` from `tree`, this checkout, its vendored crates
/// and `plan`, and return its path.
fn stage(tree: &Path, work: &Path, plan: Option<&Path>) -> Result<PathBuf> {
    let stage = work.join("stage");
    if stage.exists() {
        std::fs::remove_dir_all(&stage)?;
    }
    std::fs::create_dir_all(&stage)?;
    println!("  staging the volume in {}", stage.display());
    // A copy, sharing extents where the host's filesystem can. Not hard
    // links: mkfs.btrfs before 6.17 gives each file the link count it has
    // on the host, two, where the image holds one name, and `btrfs check`
    // then refuses the volume for files the guest never touched.
    let mut command = Command::new("cp");
    let _ = command
        .args(["-a", "--reflink=auto"])
        .arg(tree.join("."))
        .arg(&stage);
    cargo::run(command, "copying the toolchain tree")?;
    let copied = copy_sources(&stage.join("src"))?;
    println!("  {copied} tracked files in src/");
    let config = vendor(&stage.join("vendor"), plan.is_some())?;
    std::fs::create_dir_all(stage.join("cargo-home"))?;
    std::fs::write(stage.join("cargo-home/config.toml"), config)?;
    std::fs::create_dir_all(stage.join("home"))?;
    if let Some(plan) = plan {
        carry_plan(plan, &stage.join("plan"))?;
    }

    let volume = work.join("volume.img");
    let _ = std::fs::remove_file(&volume);
    let room = if plan.is_some() { PLAN_ROOM } else { ROOM };
    let size = used(&stage)?.saturating_add(room);
    std::fs::File::create(&volume)?.set_len(size)?;
    let mut command = Command::new("mkfs.btrfs");
    let _ = command.arg("-q").arg("--rootdir").arg(&stage).arg(&volume);
    cargo::run(command, "mkfs.btrfs --rootdir")?;
    println!("  volume {} ({} MiB, sparse)", volume.display(), size >> 20);
    Ok(volume)
}

/// Copy every file git tracks in this checkout, and in each component's
/// checkout at its path, as the work trees have them, into `into`, and say
/// how many.
///
/// The components are files there, with no `.git`, like the tree around
/// them: the guest has no network to clone them, and a tree that is not a
/// git checkout is not one xtask brings components into
/// ([`crate::components::ensure`]).
fn copy_sources(into: &Path) -> Result<usize> {
    let root = paths::workspace_root();
    let mut copied = copy_tracked(&root, into)?;
    for component in crate::components::manifest()? {
        if component.commit.is_none() {
            continue;
        }
        let checkout = crate::components::checkout(&component);
        if !checkout.join(".git").exists() {
            return Err(Error::new(format!(
                "{} is not checked out at {}: run any xtask command to clone it",
                component.name, component.path
            )));
        }
        copied += copy_tracked(&checkout, &into.join(&component.path))?;
    }
    Ok(copied)
}

/// Copy every file git tracks in the checkout at `root` into `into`, and say
/// how many.
fn copy_tracked(root: &Path, into: &Path) -> Result<usize> {
    let listed = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|error| Error::new(format!("running git ls-files: {error}")))?;
    if !listed.status.success() {
        return Err(Error::new(format!(
            "git ls-files failed in {}: the source is what git tracks there",
            root.display()
        )));
    }
    let mut copied = 0;
    for name in listed.stdout.split(|&byte| byte == 0) {
        if name.is_empty() {
            continue;
        }
        let name = std::str::from_utf8(name)
            .map_err(|_| Error::new("git tracks a path that is not UTF-8"))?;
        let from = root.join(name);
        let to = into.join(name);
        // Deleted in the work tree and not yet committed: absent here too.
        let Ok(meta) = std::fs::symlink_metadata(&from) else {
            continue;
        };
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if meta.file_type().is_symlink() {
            link(&std::fs::read_link(&from)?, &to)?;
        } else if meta.is_file() {
            let _ = std::fs::copy(&from, &to)
                .map_err(|error| Error::new(format!("copying {name}: {error}")))?;
        } else {
            return Err(Error::new(format!(
                "git tracks {name}, which is neither a file nor a link"
            )));
        }
        copied += 1;
    }
    Ok(copied)
}

/// A symbolic link at `to` reading `target`.
#[cfg(unix)]
fn link(target: &Path, to: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(target, to)?)
}

/// A symbolic link at `to` reading `target`: never asked for, since
/// [`test_selfhost`] refuses a host that is not Linux first.
#[cfg(not(unix))]
fn link(_target: &Path, to: &Path) -> Result<()> {
    Err(Error::new(format!(
        "{} is a symbolic link, which this host cannot make",
        to.display()
    )))
}

/// `cargo vendor` the workspace's crates.io dependencies into `into`, from
/// Cargo's own cache: no network here either, and return the Cargo
/// configuration the guest uses them with. With `every`, the crates of the
/// [`WORKSPACES`] beside it too, and of the uutils projects a plan's builds
/// compile, which are the ones `cargo xtask uutils` unpacked last.
fn vendor(into: &Path, every: bool) -> Result<String> {
    let root = paths::workspace_root();
    let mut command = Command::new(cargo::cargo());
    let _ = command
        .current_dir(&root)
        .args(["vendor", "--locked", "--offline"]);
    if every {
        let mut manifests: Vec<PathBuf> = WORKSPACES
            .iter()
            .map(|workspace| root.join(workspace).join("Cargo.toml"))
            .collect();
        manifests.extend(script_manifests());
        for manifest in manifests {
            let _ = command.arg("--sync").arg(manifest);
        }
    }
    // Not `--quiet`, which keeps the configuration from being printed too;
    // what it says as it goes is shown only when it fails.
    let output = command
        .arg(into)
        .output()
        .map_err(|error| Error::new(format!("could not run cargo vendor: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "cargo vendor --offline failed: the crates are taken from Cargo's cache, which \
             `cargo fetch --locked --manifest-path <each manifest>` fills once, with the \
             network. It said:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    // What `cargo vendor` prints is the configuration that uses what it
    // vendored, git sources included, with this machine's path in it.
    let printed =
        String::from_utf8_lossy(&output.stdout).replace(&into.display().to_string(), VENDOR);
    Ok(printed + OFFLINE)
}

/// The manifest of every Rust project a script build compiles -- each uutils
/// project and the sshdt port, as their scripts last unpacked them under the
/// data directory -- whose lockfiles name the crates it needs.
fn script_manifests() -> Vec<PathBuf> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    let data = home.join(".local/share/ferrix");
    let mut manifests: Vec<PathBuf> = std::fs::read_dir(data.join("uutils/ferrousli/build"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("Cargo.toml"))
        .collect();
    manifests.push(data.join("ports/ferrousli/sshdt/build/Cargo.toml"));
    manifests.retain(|manifest| manifest.is_file());
    manifests.sort();
    manifests
}

/// The bytes `du` says `path` holds, each hard-linked file once.
fn used(path: &Path) -> Result<u64> {
    let output = Command::new("du")
        .arg("-sb")
        .arg(path)
        .output()
        .map_err(|error| Error::new(format!("running du: {error}")))?;
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .and_then(|bytes| bytes.parse().ok())
        .ok_or_else(|| Error::new(format!("du could not size {}", path.display())))
}

/// Copy the plan in `plan` -- the list, the files it carries, and the store
/// an earlier run left, whose builds are not made again -- to `into`.
fn carry_plan(plan: &Path, into: &Path) -> Result<()> {
    std::fs::create_dir_all(into.join("files"))?;
    let _ = std::fs::copy(plan.join("plan"), into.join("plan")).map_err(|error| {
        Error::new(format!(
            "reading the plan in {}: {error}; FERRIX_BUILDS=record:{} writes one",
            plan.display(),
            plan.display()
        ))
    })?;
    let files = plan.join("files");
    if files.is_dir() {
        for entry in std::fs::read_dir(&files)? {
            let entry = entry?;
            let _ = std::fs::copy(entry.path(), into.join("files").join(entry.file_name()))?;
        }
    }
    let store = plan.join("store");
    if store.is_dir() {
        let mut command = Command::new("cp");
        let _ = command.args(["-a", "--reflink=auto"]).arg(&store).arg(into);
        cargo::run(command, "copying the store of an earlier run")?;
    }
    Ok(())
}

/// Take the store the guest made out of `volume` into `plan/store`, and
/// return where it is.
fn restore_store(volume: &Path, plan: &Path) -> Result<PathBuf> {
    let restored = plan.join("restored");
    if restored.exists() {
        std::fs::remove_dir_all(&restored)?;
    }
    std::fs::create_dir_all(&restored)?;
    let mut command = Command::new("btrfs");
    let _ = command
        .args(["restore", "--path-regex", "^/(|plan(|/store(|/.*)))$"])
        .arg(volume)
        .arg(&restored)
        .stdout(Stdio::null());
    cargo::run(command, "btrfs restore")?;
    let store = plan.join("store");
    if store.exists() {
        std::fs::remove_dir_all(&store)?;
    }
    std::fs::rename(restored.join("plan/store"), &store).map_err(|error| {
        Error::new(format!(
            "btrfs restore did not give the store ({error}): the guest said it made every \
             build, and the volume does not have them"
        ))
    })?;
    std::fs::remove_dir_all(&restored)?;
    let builds = std::fs::read_dir(&store)?.count();
    println!(
        "  took {builds} builds' outputs out of the volume into {}",
        store.display()
    );
    Ok(store)
}

/// Take the image and the kernel ELF the guest built out of `volume`, into
/// `work`, and return their paths.
fn restore(arch: Arch, volume: &Path, work: &Path, release: bool) -> Result<(PathBuf, PathBuf)> {
    let out = work.join("out");
    if out.exists() {
        std::fs::remove_dir_all(&out)?;
    }
    std::fs::create_dir_all(&out)?;
    let profile = if release { "release" } else { "debug" };
    // `btrfs restore` matches each directory on the way down as well, so the
    // expression names every one.
    let wanted = wanted(arch, profile);
    let mut command = Command::new("btrfs");
    let _ = command
        .args(["restore", "--path-regex", &wanted])
        .arg(volume)
        .arg(&out)
        .stdout(Stdio::null());
    cargo::run(command, "btrfs restore")?;
    let image = out.join(format!("src/build/{arch}/ferrix.img"));
    let kernel = out.join(format!(
        "target/{}/{profile}/ferrix-kernel",
        arch.kernel_target()
    ));
    for file in [&image, &kernel] {
        if !file.is_file() {
            return Err(Error::new(format!(
                "btrfs restore did not give {}: the guest said it built the image, and the \
                 volume does not have it",
                file.display()
            )));
        }
    }
    println!(
        "  took the guest's image out of the volume: {} MiB, kernel {} KiB",
        std::fs::metadata(&image)?.len() >> 20,
        std::fs::metadata(&kernel)?.len() >> 10
    );
    Ok((image, kernel))
}

/// The `btrfs restore` expression for `arch`'s image and kernel: it matches
/// each directory on the way down as well, so it names every one.
fn wanted(arch: Arch, profile: &str) -> String {
    format!(
        "^/(|src(|/build(|/{arch}(|/ferrix\\.img)))|target(|/{}(|/{profile}(|/ferrix-kernel))))$",
        arch.kernel_target().replace('.', "\\.")
    )
}

/// Judge a volume Ferrix built on somewhere else -- the Pixel 7's crosvm
/// guest, by `tools/vendor/google/pixel7/selfhost.sh` -- as the build boot's
/// volume is judged: a clean btrfs, an image and kernel in it, and the image
/// passing the boot test.
fn judge_volume(volume: &Path, args: &Args) -> Result<()> {
    let [arch] = args.arches()?[..] else {
        return Err(Error::new(
            "test-selfhost --volume judges one architecture: name it with --arch",
        ));
    };
    if !volume.is_file() {
        return Err(Error::new(format!("no volume at {}", volume.display())));
    }
    btrfs_check::Checker::required()?.run(volume, arch)?;
    let work = paths::build_dir(arch).join("selfhost-volume");
    let (built, built_kernel) = restore(arch, volume, &work, args.release)?;
    println!(
        "  {arch}: booting the image Ferrix built on {}",
        volume.display()
    );
    qemu::test_boot(arch, &built, &built_kernel, args)?;
    println!("  {arch}: the image Ferrix built booted");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_build_that_wrote_its_image_passes() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            "cargo 1.97.1 (5ee8cd0dd 2026-07-09)",
            BUILT,
            "  init     the shell exited with 20",
        ]);
        assert!(judge(Arch::X86_64, &lines, false).is_ok());
    }

    #[test]
    fn a_plan_is_judged_by_its_own_line() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            "builds: all 57 made",
            "  init     the shell exited with 20",
        ]);
        assert!(judge(Arch::X86_64, &lines, true).is_ok());
        assert!(judge(Arch::X86_64, &lines, false).is_err());
        let failed = transcript(&[qemu::SUCCESS_MARKER, "  init     the shell exited with 4"]);
        let error = judge(Arch::X86_64, &failed, true).unwrap_err().to_string();
        assert!(error.contains("builds: ... failed"), "{error}");
    }

    #[test]
    fn the_image_line_must_come_after_the_boot() {
        let lines = transcript(&[
            BUILT,
            qemu::SUCCESS_MARKER,
            "  init     the shell exited with 20",
        ]);
        assert!(judge(Arch::X86_64, &lines, false).is_err());
    }

    #[test]
    fn each_failing_step_is_named() {
        for (status, words) in [("3", "cargo -V"), ("4", "cargo xtask build")] {
            let exit = format!("  init     the shell exited with {status}");
            let lines = transcript(&[qemu::SUCCESS_MARKER, &exit]);
            let error = judge(Arch::X86_64, &lines, false).unwrap_err().to_string();
            assert!(error.contains(words), "{error}");
        }
    }

    #[test]
    fn the_script_builds_the_profile_asked_for() {
        assert!(script(false, false).contains("cargo xtask build --arch x86_64 ||"));
        assert!(script(true, false).contains("cargo xtask build --arch x86_64 --release ||"));
        assert!(script(false, false).contains(&format!("exit {STATUS}")));
        assert!(script(false, true).contains("cargo xtask builds-execute --plan /data/plan ||"));
    }

    #[test]
    fn the_image_line_is_the_one_xtask_prints() {
        // `main.rs`'s `build` prints "built <image>", and the image is
        // `paths::build_dir`'s `ferrix.img` under the guest's source.
        assert_eq!(BUILT, format!("built {SOURCE}/build/x86_64/ferrix.img"));
    }

    #[test]
    fn restore_names_each_architectures_image_and_kernel_and_the_directories_down() {
        let x86 = wanted(Arch::X86_64, "debug");
        assert_eq!(
            x86,
            r"^/(|src(|/build(|/x86_64(|/ferrix\.img)))|target(|/x86_64-unknown-none(|/debug(|/ferrix-kernel))))$"
        );
        let arm = wanted(Arch::AArch64, "release");
        assert!(arm.contains(r"/build(|/aarch64(|/ferrix\.img))"));
        assert!(arm.contains(r"/aarch64-unknown-none-softfloat(|/release(|/ferrix-kernel))"));
    }
}
