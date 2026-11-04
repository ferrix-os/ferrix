//! `cargo xtask test-driver-model`: the rule "writing a driver never needs a
//! change to the kernel" (customer, 2026-10-25), as a check
//! (`docs/DRIVER-MODEL.md` §8; the consultant's D1 and D20, ledger 4475).
//!
//! 1. **The kernel is built once** and its ELF's sha256 recorded, before
//!    anything of the test exists in an image.
//! 2. **The tree is QEMU's own**, dumped with `-machine dumpdtb` from the
//!    very machine the boot runs on, then edited: `virt`'s PL031 real-time
//!    clock, a device QEMU really models, is given the made-up compatible
//!    [`COMPATIBLE`], which no line of the kernel names. Its registers, its
//!    SPI and its `apb_pclk` clock reference stay as QEMU wrote them.
//! 3. **The image differs only where the rule allows**: its initramfs
//!    carries `testseconds`, and `devmgr` is built with its table row for
//!    the compatible; the tree is the edited one, given back with `-dtb` on
//!    the device-tree path (`acpi=off`). The boot carries
//!    `ferrix.unisolated`, the unclaimed word, which is what admits an
//!    untranslated generic node on AArch64 (G4 (c); D6 is the customer's
//!    open question, so no exemption entry is used).
//! 4. **The kernel the image carries is the kernel built**: its bytes, as
//!    written into the image, hash to what step 1 recorded.
//! 5. **The device works**: the kernel publishes the node generically,
//!    devmgr matches it by compatible and checks the driver's requirement
//!    table, and the driver reads the PL031's identification through its own
//!    aperture, sees the counter move, and takes the match interrupt on its
//!    own vector.
//! 6. **A missing provider is refused**: the same driver on a tree whose
//!    `clocks` names a node that does not exist is not started, and devmgr's
//!    line names the reference it lacks (D13).
//!
//! Two constants are what `gate.sh control` turns to show the check can
//! fail (D20): [`CHANGE_ONE_KERNEL_BYTE`], which makes the image carry the
//! kernel with one byte changed after its hash was recorded, and
//! [`BREAK_THE_CLOCK`], whose `false` gives the second boot its provider back.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, sha256};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-driver-model",
    help_at: 1221,
    help: &[
        "test-driver-model  Serve a device the kernel never named on an unchanged kernel",
        "              (docs/DRIVER-MODEL.md section 8): QEMU virt's PL031 under a made-up",
        "              compatible, the kernel ELF's hash held, and a missing provider refused",
    ],
    run: |_, args| test_driver_model(args),
}];

/// The made-up compatible the PL031's node is given.
const COMPATIBLE: &str = "ferrix,test-seconds";
/// The node QEMU `virt` writes for its PL031, and its path.
const NODE: &str = "pl031@9010000";
/// A phandle no node of QEMU's tree has: the second boot's clock.
const NO_SUCH_PHANDLE: &str = "0xdead0001";

/// Set to `true` only by `gate.sh control`: the image carries the kernel
/// with its last byte changed after the hash was recorded, so the hash
/// comparison must fail (D20).
const CHANGE_ONE_KERNEL_BYTE: bool = false;
/// Whether the second boot's tree names a clock provider that does not
/// exist. `gate.sh control` sets it `false`, so that boot must fail to show
/// the refusal.
const BREAK_THE_CLOCK: bool = true;

/// The boot's last line.
const BOOTED: &str = "FERRIX-BOOT-OK";
/// How long a boot reads on after [`BOOTED`] for the lines it needs.
const PATIENCE: Duration = Duration::from_secs(20);

/// The lines the serving boot must show, in this order.
const SERVED: [&str; 6] = [
    " device-tree nodes published by compatible ",
    "devmgr   testseconds /pl031@9010000: matched by compatible ferrix,test-seconds",
    "testseconds: /pl031@9010000 reads as a PL031 through its own aperture",
    "testseconds: the counter moved from ",
    "testseconds: the match interrupt came after ",
    "testseconds: /pl031@9010000 served by a driver the kernel never named",
];

/// devmgr's line for the second boot.
const REFUSED: &str = "devmgr   testseconds /pl031@9010000 not started: it needs clock apb_pclk \
                       (clocks), which names a node that is missing or disabled";

/// Which boot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// The node as QEMU wrote it, under the made-up compatible.
    Serves,
    /// The same, its clock's provider gone.
    ProviderAbsent,
}

/// Whether the row's lines are what it must show.
///
/// # Errors
///
/// What was missing or out of place.
pub(crate) fn judge(row: Row, lines: &[String]) -> std::result::Result<(), String> {
    let find = |text: &str| lines.iter().position(|line| line.contains(text));
    match row {
        Row::Serves => {
            let mut last = 0;
            for text in SERVED {
                let at = find(text).ok_or_else(|| format!("no line `{}`", text.trim()))?;
                if at < last {
                    return Err(format!("`{}` came out of order", text.trim()));
                }
                last = at;
            }
            let published = lines
                .iter()
                .find(|line| line.contains(" device-tree nodes published by compatible "))
                .and_then(|line| line.split("  generic  ").nth(1))
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|count| count.parse::<usize>().ok());
            if !published.is_some_and(|count| count > 0) {
                return Err("the kernel published no generic node".to_owned());
            }
            Ok(())
        }
        Row::ProviderAbsent => {
            let _ = find(REFUSED).ok_or_else(|| format!("no line `{REFUSED}`"))?;
            if let Some(line) = lines.iter().find(|line| line.contains("testseconds: ")) {
                return Err(format!(
                    "testseconds ran without its clock's provider: `{}`",
                    line.trim()
                ));
            }
            Ok(())
        }
    }
}

/// Edit QEMU's tree, as `dtc` writes it in source form: the PL031's node
/// takes [`COMPATIBLE`] alone, and with `break_clock` its `clocks` names
/// [`NO_SUCH_PHANDLE`].
///
/// # Errors
///
/// No PL031 node, or one without `compatible` or `clocks`.
pub(crate) fn edit(source: &str, break_clock: bool) -> std::result::Result<String, String> {
    let start = source
        .find(&format!("{NODE} {{"))
        .ok_or_else(|| format!("QEMU's tree has no {NODE}"))?;
    let (before, rest) = source.split_at(start);
    let end = rest
        .find("};")
        .ok_or_else(|| format!("{NODE} never closes"))?;
    let (body, after) = rest.split_at(end);
    let mut node = String::new();
    let (mut compatible, mut clocks) = (false, false);
    for line in body.lines() {
        let trimmed = line.trim_start();
        let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
        if trimmed.starts_with("compatible = ") {
            node.push_str(&format!("{indent}compatible = \"{COMPATIBLE}\";\n"));
            compatible = true;
        } else if trimmed.starts_with("clocks = ") {
            clocks = true;
            if break_clock {
                node.push_str(&format!("{indent}clocks = <{NO_SUCH_PHANDLE}>;\n"));
            } else {
                node.push_str(line);
                node.push('\n');
            }
        } else {
            node.push_str(line);
            node.push('\n');
        }
    }
    if !compatible || !clocks {
        return Err(format!("{NODE} has no compatible or no clocks"));
    }
    Ok(format!("{before}{node}{after}"))
}

/// Run `dtc` with `arguments`.
fn dtc(arguments: &[&str]) -> Result<()> {
    let status = Command::new("dtc")
        .args(arguments)
        .status()
        .map_err(|error| {
            Error::new(format!("could not run dtc (device-tree-compiler): {error}"))
        })?;
    if !status.success() {
        return Err(Error::new(format!("dtc {} failed", arguments.join(" "))));
    }
    Ok(())
}

/// The edited tree for `row`, from QEMU's dumped `blob`, at `out`.
fn edited_tree(blob: &Path, row: Row, out: &Path) -> Result<()> {
    let source = blob.with_extension("dts");
    dtc(&[
        "-q",
        "-I",
        "dtb",
        "-O",
        "dts",
        "-o",
        &display(&source),
        &display(blob),
    ])?;
    let text = std::fs::read_to_string(&source)?;
    let edited = edit(&text, row == Row::ProviderAbsent && BREAK_THE_CLOCK).map_err(Error::new)?;
    let edited_source = out.with_extension("dts");
    std::fs::write(&edited_source, edited)?;
    dtc(&[
        "-q",
        "-I",
        "dts",
        "-O",
        "dtb",
        "-o",
        &display(out),
        &display(&edited_source),
    ])
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

/// The image for the test: the tree's native programs with `devmgr` built
/// with its row and `testseconds` beside them, around `kernel`. The kernel
/// it carries, as written into the image, comes back with it.
fn test_image(arch: Arch, args: &Args, kernel: &cargo::Kernel) -> Result<(PathBuf, Vec<u8>)> {
    native::allow_test_driver();
    let mut natives = native::build(arch, args.release)?;
    let target = paths::target_dir().join("driver-model");
    let devmgr = natives
        .iter_mut()
        .find(|built| built.name == "devmgr")
        .ok_or_else(|| Error::new("the native programs carry no devmgr"))?;
    let path = cargo::build_native_with(
        arch,
        args.release,
        "ferrix-devmgr",
        "devmgr",
        &target,
        &[native::TEST_DEVMGR],
    )?;
    devmgr.bytes = std::fs::read(&path)?;
    natives.push(native::build_one(
        arch,
        args.release,
        &target,
        &native::TEST_DRIVER,
    )?);
    let loader = cargo::build_loader(arch, args.release)?;
    let carried = if CHANGE_ONE_KERNEL_BYTE {
        let mut bytes = std::fs::read(&kernel.elf)?;
        if let Some(last) = bytes.last_mut() {
            *last ^= 0xFF;
        }
        let changed = target.join("ferrix-kernel.changed");
        std::fs::write(&changed, bytes)?;
        cargo::Kernel::plain(changed)
    } else {
        cargo::Kernel::plain(kernel.elf.clone())
    };
    fat::keep_parts(true);
    let cmdline = crate::arch_cmdline(arch, args);
    let image = fat::write_image_with(
        arch,
        &loader,
        &carried,
        &initramfs::build(None, &natives, None, &[])?,
        cmdline.as_deref(),
    )?;
    fat::keep_parts(false);
    let in_image = std::fs::read(fat::parts_dir(arch).join("KERNEL.ELF"))?;
    Ok((image, in_image))
}

/// Boot `row`, and its lines, judged.
fn boot(arch: Arch, args: &Args, image: &Path, kernel: &Path, row: Row) -> Result<()> {
    let directory = paths::build_dir(arch).join("driver-model");
    std::fs::create_dir_all(&directory)?;
    let dumped = directory.join("qemu.dtb");
    let _ = std::fs::remove_file(&dumped);
    let mut dump = args.clone();
    dump.dump_device_tree = Some(dumped.clone());
    qemu::dump_device_tree(arch, image, kernel, &dump)?;
    let tree = directory.join(match row {
        Row::Serves => "serves.dtb",
        Row::ProviderAbsent => "provider-absent.dtb",
    });
    edited_tree(&dumped, row, &tree)?;
    let mut booted = args.clone();
    booted.device_tree = Some(tree);
    let mut lines = Vec::new();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let before = watching.lines().to_vec();
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            let mut all = before.clone();
            all.extend_from_slice(seen);
            judge(row, &all).is_ok()
        })?;
        lines = before;
        lines.extend_from_slice(watching.after());
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(arch, image, kernel, &booted, BOOTED, hook)?;
    judge(row, &lines)
        .map_err(|why| Error::new(format!("{arch}: test-driver-model row {row:?}: {why}")))?;
    println!("  {arch}: test-driver-model row {row:?} passed");
    Ok(())
}

/// Set false by the landing that starts `devmgr` on a `personality=user` kernel.
const REFUSED_UNTIL_A_USER_KERNEL_STARTS_DEVMGR: bool = true;

/// The check.
///
/// Verifies: H.DEV.7, L.device.64
fn test_driver_model(args: &Args) -> Result<()> {
    // Generic publication is `personality=user`'s (`docs/microkernel/K8.md`
    // §7), and that kernel starts no driver yet: this never goes green on a
    // kernel that publishes nothing (ledger 5896, C.2).
    if REFUSED_UNTIL_A_USER_KERNEL_STARTS_DEVMGR {
        return Err(Error::new(
            "test-driver-model: refused until a personality=user kernel starts devmgr: \
             generic publication is that kernel's; the landing that starts it owes this row",
        ));
    }
    for arch in args.arches()? {
        if arch != Arch::AArch64 {
            println!("  {arch}: test-driver-model boots AArch64's virt alone; skipped");
            continue;
        }
        // 1. The kernel, built once, and its hash recorded.
        let kernel = cargo::build_kernel(arch, args.release)?;
        let recorded = sha256::hex(&std::fs::read(&kernel.elf)?);
        println!("  {arch}: kernel built once, sha256 {recorded} recorded");
        let mut args = args.clone();
        args.kernel_options.push("ferrix.unisolated".to_owned());
        // 3. The image, around that kernel.
        let (image, carried) = test_image(arch, &args, &kernel)?;
        // 4. The kernel the image carries is the one built.
        let booted = sha256::hex(&carried);
        if booted != recorded {
            return Err(Error::new(format!(
                "{arch}: the image carries a kernel of sha256 {booted}, not the kernel built \
                 ({recorded}): the kernel changed"
            )));
        }
        println!("  {arch}: the image carries the kernel built, sha256 {booted}, unchanged");
        // 5 and 6.
        boot(arch, &args, &image, &kernel.elf, Row::Serves)?;
        boot(arch, &args, &image, &kernel.elf, Row::ProviderAbsent)?;
        println!("  {arch}: test-driver-model passed on an unchanged kernel ({recorded})");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter()
            .map(|line| format!("   1.00 | {line}"))
            .collect()
    }

    const TREE: &str = "/ {\n\tpl031@9010000 {\n\t\tclock-names = \"apb_pclk\";\n\t\tclocks = \
                        <0x8000>;\n\t\tinterrupts = <0x00 0x02 0x04>;\n\t\treg = <0x00 0x9010000 \
                        0x00 0x1000>;\n\t\tcompatible = \"arm,pl031\\0arm,primecell\";\n\t};\n};\n";

    #[test]
    fn the_edit_renames_the_node_and_breaks_only_the_asked_for_clock() {
        let served = edit(TREE, false).unwrap();
        assert!(served.contains("compatible = \"ferrix,test-seconds\";"));
        assert!(!served.contains("arm,pl031"));
        assert!(served.contains("clocks = <0x8000>;"));
        let broken = edit(TREE, true).unwrap();
        assert!(broken.contains("clocks = <0xdead0001>;"));
        assert!(broken.contains("reg = <0x00 0x9010000"));
        assert!(edit("/ { };", false).is_err());
    }

    #[test]
    fn a_serving_boot_needs_every_line_in_order() {
        // A node's own line comes first, as it does on a boot.
        let mut all: Vec<&str> = vec![
            "  generic  published /pl031@9010000",
            "  generic  6 device-tree nodes published by compatible (1 the kernel depends on)",
        ];
        all.extend_from_slice(&SERVED[1..]);
        assert_eq!(judge(Row::Serves, &lines(&all)), Ok(()));
        let mut missing = all.clone();
        let _ = missing.remove(5);
        assert!(judge(Row::Serves, &lines(&missing)).is_err());
        let mut none = all.clone();
        none[1] =
            "  generic  0 device-tree nodes published by compatible (0 the kernel depends on)";
        assert!(judge(Row::Serves, &lines(&none)).is_err());
        let mut swapped = all;
        swapped.swap(3, 4);
        assert!(judge(Row::Serves, &lines(&swapped)).is_err());
    }

    #[test]
    fn a_missing_provider_must_be_refused_and_never_run() {
        assert_eq!(judge(Row::ProviderAbsent, &lines(&[REFUSED])), Ok(()));
        assert!(judge(Row::ProviderAbsent, &lines(&[])).is_err());
        assert!(
            judge(
                Row::ProviderAbsent,
                &lines(&[REFUSED, "testseconds: the counter moved from 1 to 2"])
            )
            .is_err()
        );
    }
}
