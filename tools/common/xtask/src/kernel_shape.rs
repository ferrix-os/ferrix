//! `cargo xtask test-kernel-shape --personality user`: what ring 0 holds in the
//! microkernel's shape (`docs/microkernel/M6.md` §3): rustc's own list of what it
//! read for the kernel's crates, and the kernel's symbols, against M6's list.

use crate::args::Args;
use crate::check::python_with;
use crate::paths;
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu};

/// The task.
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-kernel-shape",
        help_at: 965,
        help: &[
            "test-kernel-shape  Build --personality user and hold ring 0 to M6's list: no load file, none of what left, by rustc's dep-info and nm",
        ],
        run: |_, args| test_kernel_shape(args),
    },
    Task {
        name: "test-ring0-budget",
        help_at: 967,
        help: &[
            "test-ring0-budget  Ring 0's size against tools/common/data/ring0-budget.json: six release kernels, three flag sets on",
            "              x86_64 and aarch64; a rise fails, a fall past 1% with no edit fails (docs/microkernel/TINY-KERNEL.md §7)",
        ],
        run: |_, _| test_ring0_budget(),
    },
    Task {
        name: "test-kernel-symbols",
        help_at: 966,
        help: &[
            "test-kernel-symbols  With FERRIX_SYMBOLS_BASE=<rev>: this kernel's symbols and sizes against that commit's, for what is said to leave a build unchanged (M.8)",
        ],
        run: |_, args| test_kernel_symbols(args),
    },
];

/// M.8: this tree's kernel, by this run's flags, against the commit `FERRIX_SYMBOLS_BASE`
/// names, built from a worktree of its own with the same flags.
fn test_kernel_symbols(args: &Args) -> Result<()> {
    let base = std::env::var("FERRIX_SYMBOLS_BASE")
        .map_err(|_| Error::new("test-kernel-symbols needs FERRIX_SYMBOLS_BASE=<rev>"))?;
    let target = paths::target_dir();
    let tree = target.join("symbols-base");
    let _ = std::fs::remove_dir_all(&tree);
    let added = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "--force"])
        .arg(&tree)
        .arg(&base)
        .status()
        .map_err(|error| Error::new(error.to_string()))?;
    if !added.success() {
        return Err(Error::new(format!("no worktree of {base}")));
    }
    for arch in args.arches()? {
        let kernel = cargo::build_kernel(arch, args.release)?;
        let tail = kernel
            .elf
            .strip_prefix(&target)
            .map_err(|error| Error::new(error.to_string()))?;
        let base_target = tree.join("target-symbols");
        let built = std::process::Command::new("cargo")
            .args(["xtask", "build", "--arch", arch.name()])
            .args(["--kernel-profile", &crate::flags::asked_profile()])
            .args(args.release.then_some("--release"))
            .args(
                args.sets
                    .iter()
                    .flat_map(|(flag, value)| ["--set".to_owned(), format!("{flag}={value}")]),
            )
            .current_dir(&tree)
            .env("CARGO_TARGET_DIR", &base_target)
            .status()
            .map_err(|error| Error::new(error.to_string()))?;
        if !built.success() {
            return Err(Error::new(format!("{base} did not build for {arch}")));
        }
        let (head, then) = (
            kernel.elf.display().to_string(),
            base_target.join(tail).display().to_string(),
        );
        python_with(
            "tools/common/check/compare-kernel-symbols.py",
            &[then.as_str(), head.as_str()],
        )?;
        println!("  {arch}: the kernel's symbols and sizes are {base}'s");
    }
    let _ = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&tree)
        .status();
    Ok(())
}

fn test_kernel_shape(args: &Args) -> Result<()> {
    if crate::flags::this_run()?.value("personality") != Some("user") {
        return Err(Error::new("test-kernel-shape is for --personality user"));
    }
    for arch in args.arches()? {
        let kernel = cargo::build_kernel(arch, args.release)?;
        let directory = kernel
            .elf
            .parent()
            .ok_or_else(|| Error::new("the kernel has no directory"))?
            .display()
            .to_string();
        python_with(
            "tools/common/check/check-kernel-shape.py",
            &[directory.as_str(), "--report"],
        )?;
        python_with(
            "tools/common/check/check-kernel-alloc-audit.py",
            &["--built", directory.as_str()],
        )?;
        // Z3 and 6093 (9)(d): what K1's fast path promises of normal mode's code.
        if arch == paths::Arch::X86_64 {
            let linuxd = native::LINUXD;
            let path = cargo::build_native(
                arch,
                args.release,
                linuxd.package,
                linuxd.binary,
                &paths::target_dir(),
            )?;
            let path = path.display().to_string();
            python_with(
                "tools/common/check/check-linuxd-binary.py",
                &[path.as_str()],
            )?;
        }
        if let Ok(set) = std::env::var(BUDGET_SET) {
            if !args.release {
                return Err(Error::new("the ring-0 budget reads release images (M.7)"));
            }
            python_with(
                "tools/common/check/check-kernel-shape.py",
                &[directory.as_str(), "--budget", &set, "--arch", arch.name()],
            )?;
        }
    }
    Ok(())
}

/// The environment variable by which [`test_ring0_budget`] tells each child build its set.
const BUDGET_SET: &str = "FERRIX_RING0_BUDGET_SET";

/// `test-ring0-budget`: every build `ring0-budget.json` lists, each a child `test-kernel-shape
/// --release` with its `--set` values, so that each resolves its flags afresh; then that
/// build held to its baseline. Every build runs, and the row fails if any did.
fn test_ring0_budget() -> Result<()> {
    let interpreter = crate::check::python_interpreter().ok_or_else(|| {
        Error::new("no working Python interpreter on PATH (tried python3, python)")
    })?;
    let listed = std::process::Command::new(interpreter)
        .current_dir(paths::workspace_root())
        .args(["tools/common/check/check-kernel-shape.py", "--sets"])
        .output()
        .map_err(|error| Error::new(format!("check-kernel-shape.py --sets: {error}")))?;
    if !listed.status.success() {
        return Err(Error::new("check-kernel-shape.py --sets failed"));
    }
    let program = std::env::current_exe().map_err(|error| Error::new(error.to_string()))?;
    let started = std::time::Instant::now();
    let mut failed = Vec::new();
    let mut built = 0_usize;
    let text = String::from_utf8_lossy(&listed.stdout).into_owned();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut words = line.split_whitespace();
        let (Some(set), Some(arch)) = (words.next(), words.next()) else {
            return Err(Error::new(format!("check-kernel-shape.py --sets: {line}")));
        };
        let mut command = std::process::Command::new(&program);
        let _ = command
            .current_dir(paths::workspace_root())
            .env(BUDGET_SET, set)
            .args([
                "test-kernel-shape",
                "--arch",
                arch,
                "--personality",
                "user",
                "--release",
            ]);
        for value in words {
            let _ = command.args(["--set", value]);
        }
        built += 1;
        let begun = std::time::Instant::now();
        println!("== ring 0's budget: {line}");
        let status = command
            .status()
            .map_err(|error| Error::new(format!("{line}: {error}")))?;
        println!("== {set} {arch}: {} s", begun.elapsed().as_secs());
        if !status.success() {
            failed.push(format!("{set} {arch}"));
        }
    }
    if built == 0 {
        return Err(Error::new(
            "ring0-budget: check-kernel-shape.py --sets listed no build",
        ));
    }
    println!("ring0-budget: {} s in all", started.elapsed().as_secs());
    if failed.is_empty() {
        Ok(())
    } else {
        Err(Error::new(format!(
            "ring0-budget: failed: {}",
            failed.join(", ")
        )))
    }
}

/// `test-first-processes`: the manager, its partition, devmgr and `linuxd`
/// (`docs/microkernel/BOOT-AUTHORITY.md` §6). Test images ship in no product.
pub(crate) const FIRST_TASKS: &[Task] = &[
    Task {
        name: "test-btrfsd",
        help_at: 961,
        help: &[
            "test-btrfsd   Boot --personality user --set fs=server: btrfs in its own process, read by a program.",
        ],
        run: |_, args| test_btrfsd(args),
    },
    Task {
        name: "test-netd",
        help_at: 963,
        help: &[
            "test-netd     netd as the wire's switch (docs/microkernel/M8.md §2): killed and started again under pings, and two partitions reaching each other.",
        ],
        run: |_, args| test_netd(args),
    },
    Task {
        name: "test-partitions",
        help_at: 964,
        help: &[
            "test-partitions  Two Linux partitions (docs/microkernel/M8.md §3): one ends or dies, and the other goes on; partial.",
        ],
        run: |_, args| test_partitions(args),
    },
    Task {
        name: "test-btrfsd-restart",
        help_at: 962,
        help: &[
            "test-btrfsd-restart  As test-btrfsd, over a driver that dies, hangs and decommits: requests wait or fail by their bound, and a dead driver's memory goes.",
        ],
        run: |_, args| test_btrfsd_restart(args),
    },
];

/// The description of an `fs=server` image: `btrfsd`, the one row
/// (`docs/microkernel/BTRFSD.md` §4, BD.6).
const SERVER_ROW: &str =
    "[[partition]]\nname = \"btrfsd\"\nimage = \"sbin/btrfsd\"\nrole = \"files\"\nrestarts = 0\n";

/// The file [`SERVER_ROW`] describes, generated and judged against the images.
fn described(root: &std::path::Path, row: &str) -> Result<crate::ports::File> {
    let toml = root.join("partitions.toml");
    std::fs::write(&toml, row).map_err(|e| Error::new(e.to_string()))?;
    let out = root.join("partitions");
    let (toml, root_text, out_text) = (
        toml.display().to_string(),
        root.display().to_string(),
        out.display().to_string(),
    );
    python_with(
        "tools/common/gen/gen-partitions.py",
        &[toml.as_str(), root_text.as_str(), out_text.as_str()],
    )?;
    python_with(
        "tools/common/check/check-partitions.py",
        &[out_text.as_str(), root_text.as_str()],
    )?;
    Ok(crate::ports::File {
        path: "etc/ferrix/partitions".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(
            std::fs::read(&out).map_err(|e| Error::new(e.to_string()))?,
        ),
    })
}

/// The first page of an ELF `image` that holds something executable and anything else,
/// or none. The builder copies such a page and protects it to the union, and K14 refuses
/// an executable copy in a measured job: found here, on the bytes the image will carry,
/// rather than as a refusal at run time (ledger 5896 BA.4). A malformed image is left to
/// the loaders' own refusals and is a finding of neither.
pub(crate) fn executable_page_shared(image: &[u8]) -> Option<u64> {
    const PAGE: u64 = 4096;
    let word = |at: usize, wide: usize| -> Option<u64> {
        let bytes = image.get(at..at.checked_add(wide)?)?;
        Some(
            bytes
                .iter()
                .rev()
                .fold(0, |value, byte| value << 8 | u64::from(*byte)),
        )
    };
    if image.get(..4) != Some(b"\x7fELF") {
        return None;
    }
    let wide = image.get(4) == Some(&2);
    let (table, size, count) = if wide {
        (word(32, 8)?, word(54, 2)?, word(56, 2)?)
    } else {
        (word(28, 4)?, word(42, 2)?, word(44, 2)?)
    };
    let mut segments = Vec::new();
    for index in 0..count {
        let at = usize::try_from(table.checked_add(index.checked_mul(size)?)?).ok()?;
        if word(at, 4)? != 1 {
            continue;
        }
        let (flags, start, memory) = if wide {
            (word(at + 4, 4)?, word(at + 16, 8)?, word(at + 40, 8)?)
        } else {
            (word(at + 24, 4)?, word(at + 8, 4)?, word(at + 20, 4)?)
        };
        if memory != 0 {
            segments.push((flags & 1 != 0, start, start.checked_add(memory)?));
        }
    }
    for (index, &(executable, start, end)) in segments.iter().enumerate() {
        if !executable {
            continue;
        }
        let (first, last) = (start / PAGE, (end - 1) / PAGE);
        for (other_index, &(_, other_start, other_end)) in segments.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let (other_first, other_last) = (other_start / PAGE, (other_end - 1) / PAGE);
            if first <= other_last && other_first <= last {
                return Some(first.max(other_first) * PAGE);
            }
        }
    }
    None
}

/// Every program of a `personality=user` image goes into a measured job or may: none has
/// an executable page the builder would have to copy.
pub(crate) fn refuse_executable_copies(natives: &[native::Built]) -> Result<()> {
    if crate::flags::this_run()?.value("personality") != Some("user") {
        return Ok(());
    }
    for built in natives {
        if let Some(page) = executable_page_shared(&built.bytes) {
            return Err(Error::new(format!(
                "{}: the page at {page:#x} holds code and something else: a measured job refuses its copy (K14 §3)",
                built.name
            )));
        }
    }
    Ok(())
}

/// The description any `fs=server` image carries, its one row `btrfsd`, whose
/// digest is read from `natives` (`docs/microkernel/BTRFSD.md` §4). `None` in any
/// other build.
pub(crate) fn server_description(natives: &[native::Built]) -> Result<Option<crate::ports::File>> {
    if crate::flags::this_run()?.value("fs") != Some("server") {
        return Ok(None);
    }
    let server = natives
        .iter()
        .find(|built| built.name == "btrfsd")
        .ok_or_else(|| Error::new("btrfsd was not built"))?;
    server_description_of(&server.bytes)
}

/// [`server_description`] from `btrfsd`'s bytes as the image will hold them.
pub(crate) fn server_description_of(server: &[u8]) -> Result<Option<crate::ports::File>> {
    if crate::flags::this_run()?.value("fs") != Some("server") {
        return Ok(None);
    }
    let root = paths::target_dir().join("server-description");
    let image_dir = root.join("sbin");
    std::fs::create_dir_all(&image_dir).map_err(|e| Error::new(e.to_string()))?;
    std::fs::write(image_dir.join("btrfsd"), server).map_err(|e| Error::new(e.to_string()))?;
    described(&root, SERVER_ROW).map(Some)
}

/// `cargo xtask test-btrfsd --personality user --set fs=server --init busybox`:
/// the first tranche's row (`docs/microkernel/BTRFSD.md` §6, Q3), a read-only
/// volume from a process of its own.
fn test_btrfsd(args: &Args) -> Result<()> {
    let flags = crate::flags::this_run()?;
    if flags.value("fs") != Some("server") {
        return Err(Error::new("test-btrfsd is for --set fs=server"));
    }
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-btrfsd runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::BTRFSD_SCRIPT,
        )?;
        let natives = native::build(arch, args.release)?;
        let carried = crate::shell::btrfsd_carried(&busybox)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_btrfsd(arch, &image, &kernel, args)?;
    }
    Ok(())
}

fn net_cost(args: &Args, arch: paths::Arch, busybox: &std::path::Path) -> Result<String> {
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel =
        cargo::build_kernel_with_init(arch, args.release, busybox, crate::shell::NETD_RTT_SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let mut carried = crate::shell::netd_carried(busybox)?;
    carried.extend(crate::dhcp::carried(arch)?);
    let initramfs = initramfs::build(None, &natives, None, &carried)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
    qemu::net_round_trip(arch, &image, &kernel, args)
}

fn test_netd(args: &Args) -> Result<()> {
    let args = &Args {
        net: true,
        ..args.clone()
    };
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-netd runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let without = net_cost(args, arch, &busybox)?;
        // SAFETY: xtask is one thread here, before any build that reads them.
        unsafe { std::env::set_var("FERRIX_TEST_NETD", "1") };
        let with = net_cost(args, arch, &busybox)?;
        println!("  {arch}: a ping's min/avg/max without netd {without}, with netd {with}");
        // SAFETY: as above.
        unsafe { std::env::set_var("FERRIX_TEST_NETD_DIES_AFTER", "300") };
        let kernel =
            cargo::build_kernel_with_init(arch, args.release, &busybox, crate::shell::NETD_SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        let mut carried = crate::shell::netd_carried(&busybox)?;
        carried.extend(crate::dhcp::carried(arch)?);
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_netd_restart(arch, &image, &kernel, args)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::NETD_TWO_SCRIPT,
        )?;
        let natives = native::build(arch, args.release)?;
        let root = paths::target_dir().join("netd-image");
        let image_dir = root.join("sbin");
        std::fs::create_dir_all(&image_dir).map_err(|e| Error::new(e.to_string()))?;
        let linuxd = natives
            .iter()
            .find(|built| built.name == "linuxd")
            .ok_or_else(|| Error::new("linuxd was not built"))?;
        std::fs::write(image_dir.join("linuxd"), &linuxd.bytes)
            .map_err(|e| Error::new(e.to_string()))?;
        let row = "[[partition]]\nname = \"second\"\nimage = \"sbin/linuxd\"\nrole = \"linux\"\nreserve_pages = 40000\n";
        let mut carried = crate::shell::netd_carried(&busybox)?;
        carried.extend(crate::dhcp::carried(arch)?);
        carried.push(described(&root, row)?);
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_netd_two(arch, &image, &kernel, args)?;
    }
    Ok(())
}

fn test_partitions(args: &Args) -> Result<()> {
    // Two partitions each unpack the archive into memory of their own, and a kernel of
    // kernel_memory=retyped promises what it has up front (K16 §4).
    let args = &Args {
        memory: if args.memory_given { args.memory } else { 1024 },
        ..args.clone()
    };
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-partitions runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::PARTITIONS_SCRIPT,
        )?;
        let natives = native::build(arch, args.release)?;
        let target = paths::target_dir();
        let dies = native::build_one(arch, args.release, &target, &native::LINUXD_DIES)?;
        let root = target.join("partitions-image");
        let image_dir = root.join("sbin");
        std::fs::create_dir_all(&image_dir).map_err(|e| Error::new(e.to_string()))?;
        let linuxd = natives
            .iter()
            .find(|built| built.name == "linuxd")
            .ok_or_else(|| Error::new("linuxd was not built"))?;
        std::fs::write(image_dir.join("linuxd"), &linuxd.bytes)
            .map_err(|e| Error::new(e.to_string()))?;
        std::fs::write(image_dir.join("dies"), &dies.bytes)
            .map_err(|e| Error::new(e.to_string()))?;
        for (image, ends) in [("sbin/linuxd", false), ("sbin/dies", true)] {
            let row = format!(
                "[[partition]]\nname = \"second\"\nimage = \"{image}\"\nrole = \"linux\"\nreserve_pages = 40000\n"
            );
            let mut carried = crate::shell::lift_carried(&busybox)?;
            carried.push(described(&root, &row)?);
            if ends {
                carried.push(crate::ports::File {
                    path: "sbin/dies".to_owned(),
                    mode: 0o755,
                    content: crate::ports::Content::Bytes(dies.bytes.clone()),
                });
            }
            let initramfs = initramfs::build(None, &natives, None, &carried)?;
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
            qemu::test_partitions(arch, &image, &kernel, args, ends)?;
        }
        // The other way round (ledger 6226 N.8): what the first's death costs is shown. Its
        // `linuxd` ends at once, the second a real one, and the machine ends with it.
        let row = "[[partition]]\nname = \"second\"\nimage = \"sbin/second\"\nrole = \"linux\"\nreserve_pages = 40000\n";
        std::fs::write(image_dir.join("second"), &linuxd.bytes)
            .map_err(|e| Error::new(e.to_string()))?;
        let mut carried = crate::shell::lift_carried(&busybox)?;
        carried.push(described(&root, row)?);
        carried.push(crate::ports::File {
            path: "sbin/second".to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(linuxd.bytes.clone()),
        });
        let mut dying = natives.clone();
        dying
            .iter_mut()
            .find(|built| built.name == "linuxd")
            .ok_or_else(|| Error::new("linuxd was not built"))?
            .bytes
            .clone_from(&dies.bytes);
        let initramfs = initramfs::build(None, &dying, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_first_partition_dies(arch, &image, &kernel, args)?;
    }
    Ok(())
}

/// `test-btrfsd-restart`: L.object.332 and H.4 (`docs/microkernel/BTRFSD.md` §5),
/// with `blk-victim` under the name `blk`.
fn test_btrfsd_restart(args: &Args) -> Result<()> {
    if crate::flags::this_run()?.value("fs") != Some("server") {
        return Err(Error::new("test-btrfsd-restart is for --set fs=server"));
    }
    let init = args
        .init
        .as_deref()
        .ok_or_else(|| Error::new("test-btrfsd-restart runs busybox: --init"))?;
    for arch in args.arches()? {
        let loader = cargo::build_loader(arch, args.release)?;
        let busybox = crate::program_for(init, arch)?;
        let kernel = cargo::build_kernel_with_init(
            arch,
            args.release,
            &busybox,
            crate::shell::BTRFSD_RESTART_SCRIPT,
        )?;
        let mut natives = native::build(arch, args.release)?;
        let target = paths::target_dir();
        let victim = native::build_one(arch, args.release, &target, &native::BLK_VICTIM)?;
        natives
            .iter_mut()
            .find(|built| built.name == "blk")
            .ok_or_else(|| Error::new("the native programs carry no blk driver to replace"))?
            .bytes = victim.bytes;
        let carried = crate::shell::btrfsd_carried(&busybox)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        qemu::test_btrfsd_restart(arch, &image, &kernel, args)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::executable_page_shared;

    /// A 64-bit ELF of these (flags, address, size) load segments.
    fn elf(segments: &[(u32, u64, u64)]) -> Vec<u8> {
        let mut image = vec![0_u8; 64];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[32] = 64;
        image[54] = 56;
        image[56] = u8::try_from(segments.len()).unwrap();
        for &(flags, address, size) in segments {
            let mut header = vec![0_u8; 56];
            header[0] = 1;
            header[4..8].copy_from_slice(&flags.to_le_bytes());
            header[16..24].copy_from_slice(&address.to_le_bytes());
            header[40..48].copy_from_slice(&size.to_le_bytes());
            image.extend(header);
        }
        image
    }

    #[test]
    fn code_alone_on_its_pages_is_no_finding() {
        assert_eq!(
            executable_page_shared(&elf(&[(4, 0, 0x800), (5, 0x1000, 0x1800), (6, 0x3000, 8)])),
            None
        );
    }

    #[test]
    fn code_and_data_on_one_page_is_found() {
        assert_eq!(
            executable_page_shared(&elf(&[(4, 0, 0x1800), (5, 0x1800, 0x100)])),
            Some(0x1000)
        );
        assert_eq!(
            executable_page_shared(&elf(&[(5, 0x1000, 0x10), (6, 0x1010, 8)])),
            Some(0x1000)
        );
    }

    #[test]
    fn something_else_is_no_finding_and_no_panic() {
        assert_eq!(executable_page_shared(b"not an image"), None);
        assert_eq!(executable_page_shared(&elf(&[(5, 0x1000, 0)])), None);
        let mut short = elf(&[(5, 0x1000, 8)]);
        short.truncate(70);
        assert_eq!(executable_page_shared(&short), None);
    }
}
