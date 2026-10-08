//! `bench-seam`: the in-kernel reference for the seam's first row
//! (`docs/BACKLOG.md`, "the seam measured, 1"; `docs/OPAQUE-KERNEL.md`, S0).
//!
//! Ferrix's boot measures a 4 KiB read of the pattern disk through the block
//! ring and its ring-3 driver, at depths 1 and 32 (`src/kernel/src/interfaces/block_ring/
//! hop_check.rs`, the `seam` boot line). This boots a stock Linux kernel --
//! Debian 13's cloud kernel, fetched and pinned by
//! `tools/common/fetch/fetch-linux-reference.sh`, its driver in ring 0 -- on the
//! same QEMU machine, IOMMU and all, and reads the same disk with
//! `dd iflag=direct`: one `dd` of 1024 reads for depth 1, and 32 at once of 32
//! reads each for depth 32. Each `dd`'s time over its reads is its mean
//! request. The kernel's SHA-256 is printed beside the numbers, so that a
//! number can always be traced to the kernel that produced it.
//!
//! The comparison is the difference between the two: what Ferrix pays to put
//! its driver in ring 3, against a kernel that calls its driver.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, initramfs, qemu, sha256, test_disk};

/// Reads at each depth, as the kernel's own measurement makes.
const READS: u32 = 1024;
/// The deeper queue.
const DEPTH: u32 = 32;

/// What the reference guest runs as `/init`.
const INIT: &str = r#"#!/bin/busybox sh
/bin/busybox --install -s /bin
mount -t proc proc /proc
mount -t sysfs sys /sys
mount -t devtmpfs dev /dev
insmod /virtio_blk.ko
n=0
while [ ! -b /dev/vda ] && [ $n -lt 100 ]; do sleep 0.1; n=$((n+1)); done
dd if=/dev/vda of=/dev/null bs=4096 count=1024 iflag=direct 2>&1 | tail -1 | sed 's/^/seamref depth1 /'
i=0
while [ $i -lt 32 ]; do
  dd if=/dev/vda of=/dev/null bs=4096 count=32 skip=$((i*32)) iflag=direct 2>&1 | tail -1 | sed 's/^/seamref depth32 /' &
  i=$((i+1))
done
wait
echo seamref done
poweroff -f
"#;

/// Boot the reference kernel on each architecture asked for that has one,
/// and print its numbers.
///
/// # Errors
///
/// Files the fetch script has not fetched, a QEMU that does not start, or a
/// guest that never says it is done.
pub(crate) fn bench_seam(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            println!(
                "  {arch}: no reference kernel is fetched for ARMv7-A; the row names x86-64 and AArch64"
            );
            continue;
        }
        reference(arch, args)?;
    }
    Ok(())
}

/// `~/.local/share/ferrix`, where the fetch scripts put what they fetch.
fn share() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share/ferrix")
}

/// Where the fetch script put `arch`'s kernel and module.
fn reference_dir(arch: Arch) -> PathBuf {
    std::env::var_os("FERRIX_LINUX_REFERENCE")
        .map_or_else(|| share().join("linux-ref"), PathBuf::from)
        .join(arch.name())
}

/// Read `path`, or say what fetches it.
fn fetched(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| {
        Error::new(format!(
            "reading {}: {error}; tools/common/fetch/fetch-linux-reference.sh fetches the reference \
             kernel, and the static busybox is the one test-shell uses",
            path.display()
        ))
    })
}

/// One architecture's reference run.
fn reference(arch: Arch, args: &Args) -> Result<()> {
    let dir = reference_dir(arch);
    let kernel_path = dir.join("vmlinuz");
    let kernel = fetched(&kernel_path)?;
    let module = fetched(&dir.join("virtio_blk.ko"))?;
    let busybox = fetched(
        &share()
            .join("busybox")
            .join(arch.name())
            .join("bin/busybox.static"),
    )?;
    let digest: String = sha256::digest(&kernel)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let archive = initramfs::plain(
        &["bin", "dev", "proc", "sys"],
        &[
            ("init", 0o755, INIT.as_bytes()),
            ("bin/busybox", 0o755, &busybox),
            ("virtio_blk.ko", 0o644, &module),
        ],
    )?;
    let build = paths::workspace_root().join("build").join(arch.name());
    std::fs::create_dir_all(&build)?;
    let initrd = build.join("seam-reference.cpio");
    std::fs::write(&initrd, archive)?;

    let binary = PathBuf::from(arch.qemu_binary());
    // Emulated unless asked otherwise: the reference was measured under tcg.
    let accelerator =
        qemu::accelerator(arch, &binary, Some(args.accel.as_deref().unwrap_or("tcg")))?;
    let mut command = machine(arch, args, &binary, &accelerator)?;
    let disk = test_disk::ensure()?;
    // The pattern disk exactly as Ferrix's boots attach it.
    let _ = command.args([
        "-kernel",
        &kernel_path.display().to_string(),
        "-initrd",
        &initrd.display().to_string(),
        "-append",
        console(arch),
        "-drive",
        &format!(
            "file={},if=none,format=raw,id=testdisk,readonly=on",
            disk.display()
        ),
        "-device",
        "virtio-blk-pci,drive=testdisk,disable-legacy=on,iommu_platform=on",
    ]);
    println!(
        "  {arch}: stock Linux (vmlinuz sha256 {digest}) reading the pattern disk under \
         {accelerator}"
    );
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| Error::new(format!("starting {}: {error}", binary.display())))?;
    let text = String::from_utf8_lossy(&output.stdout);
    if !text.contains("seamref done") {
        return Err(Error::new(format!(
            "{arch}: the reference guest never finished; it said:\n{}",
            text.lines().rev().take(20).collect::<Vec<_>>().join("\n")
        )));
    }
    let one = means(&text, "seamref depth1 ", READS);
    let deep = means(&text, "seamref depth32 ", READS / DEPTH);
    match (mean(&one), mean(&deep)) {
        (Some(one), Some(deep)) => {
            println!(
                "  {arch}: seamref a 4 KiB O_DIRECT read in stock Linux: depth 1 mean {} us; \
                 depth {DEPTH} mean {} us",
                micros(one),
                micros(deep)
            );
            Ok(())
        }
        _ => Err(Error::new(format!(
            "{arch}: the reference guest printed no dd timings:\n{text}"
        ))),
    }
}

/// QEMU, with Ferrix's own machine for `arch`: q35 with a VT-d unit, or
/// `virt` with the SMMU v3, and the same CPU, memory and processors.
fn machine(arch: Arch, args: &Args, binary: &Path, accelerator: &str) -> Result<Command> {
    let mut command = Command::new(binary);
    let _ = command
        .args(qemu::accelerator_arguments(accelerator, None)?)
        .args([
            "-m",
            &args.memory.to_string(),
            "-smp",
            &qemu::processors(accelerator, binary, args).to_string(),
            "-nographic",
            "-no-reboot",
            "-monitor",
            "none",
        ]);
    if arch == Arch::X86_64 {
        let _ = command.args([
            "-machine",
            &qemu::x86_machine(accelerator),
            "-cpu",
            &qemu::x86_cpu(accelerator),
            "-device",
            qemu::INTEL_IOMMU,
        ]);
    } else {
        let _ = command
            .args(qemu::VIRT_MACHINE)
            .args(["-cpu", &qemu::arm_cpu(arch)]);
    }
    Ok(command)
}

/// The reference kernel's command line: its console, and on x86-64 the
/// VT-d unit in use, as Ferrix uses it.
const fn console(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "console=ttyS0 quiet intel_iommu=on",
        _ => "console=ttyAMA0 quiet",
    }
}

/// The mean request, in nanoseconds, of each `dd` line after `tag`: its
/// seconds over the `reads` it made.
fn means(text: &str, tag: &str, reads: u32) -> Vec<f64> {
    text.lines()
        .filter_map(|line| line.split_once(tag).map(|(_, rest)| rest))
        .filter_map(seconds)
        .map(|seconds| seconds * 1e9 / f64::from(reads.max(1)))
        .collect()
}

/// The seconds a busybox `dd` summary line reports: `N bytes (…) copied, S
/// seconds, R`.
fn seconds(line: &str) -> Option<f64> {
    let (_, after) = line.split_once("copied, ")?;
    let (number, _) = after.split_once(" seconds")?;
    number.trim().parse().ok()
}

/// The mean of `values`, if there are any.
fn mean(values: &[f64]) -> Option<f64> {
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// Nanoseconds as microseconds with one decimal.
fn micros(nanos: f64) -> String {
    format!("{:.1}", nanos / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::{means, seconds};

    #[test]
    fn a_dd_summary_says_its_seconds() {
        let line = "4194304 bytes (4.0MB) copied, 0.375204 seconds, 10.7MB/s";
        assert_eq!(seconds(line), Some(0.375_204));
        assert_eq!(seconds("dd: /dev/vda: No such device"), None);
    }

    #[test]
    fn each_dd_line_is_a_mean_request() {
        let text = "x\nseamref depth1 4194304 bytes (4.0MB) copied, 0.1024 seconds, 40MB/s\n";
        let got = means(text, "seamref depth1 ", 1024);
        assert_eq!(got.len(), 1);
        assert!((got[0] - 100_000.0).abs() < 1.0, "{got:?}");
    }
}
