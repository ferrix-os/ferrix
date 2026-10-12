//! `run-nazuna`: Ferrix on a virtual machine shaped like the customer's PC,
//! nazuna (`docs/NAZUNA-VM.md`): an ASUS ProArt B650-CREATOR with a Ryzen 9
//! 9900X, whose WD PC SN720 Ferrix was installed on and has not booted from
//! (2026-10-11).
//!
//! What is copied, from what nazuna reported on 2026-10-12:
//!
//! * the processor: KVM's `-cpu host` on nazuna itself, 12 cores of two
//!   threads, and 16 GiB unless `--memory` says (nazuna has 64);
//! * an AMD IOMMU (`amd-iommu`, described by IVRS) in place of the VT-d unit
//!   every other machine here has;
//! * the three NVMe drives, as GPT disks of their real sizes with their real
//!   partitions, sparse: the Lexar NM790 on a root port of its own, and the
//!   SN720 and the Lexar NQ790 below one switch, as the chipset's M.2 slots
//!   are. The two Lexars hold Ubuntu's and Windows' partitions, empty;
//! * the SN720's layout to the MiB, Ferrix installed on it by
//!   `ferrix-install --slots` from a live medium whose command line carries
//!   `ferrix.fbcon`, as the customer's install had; firmware then boots it
//!   as nazuna's `UEFI OS` entry does, from `EFI/BOOT/BOOTX64.EFI`;
//! * USB: an xHCI controller with a keyboard and a mouse;
//! * none of the test machine's own devices: no virtio random number
//!   generator, `edu`, `pci-testdev`, fixture disks or network.
//!
//! What is not, and cannot be by QEMU: the board's AMI firmware (OVMF runs
//! instead), its ACPI tables, the chipset's own devices, the Realtek network
//! controllers and the three graphics cards. A serial port is kept, which
//! the PC has not, so that the boot can be read.

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Command;

use ferrix_partition::{ESP, Guid, LINUX_FILESYSTEM};

use super::slots::{CONTROL_TYPE, LINUX_SWAP, SLOT_TYPE, lay};
use super::{files, write_medium_with};
use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, zinc};

/// nazuna's IOMMU: AMD-Vi, remapping interrupts. xAPIC, as the kernel runs,
/// so no x2APIC support (`xtsup`) is offered.
pub(crate) const AMD_IOMMU: &str = "amd-iommu,intremap=on";

/// The live medium's command line: `/` in memory, and the kernel's lines on
/// the screen as well, as the customer's install carried.
const LIVE_CMDLINE: &str = "ferrix.root=tmpfs ferrix.fbcon";

/// What the install boot's shell runs: the A/B install onto the SN720's
/// copy, keeping the layout it already has.
const INSTALL: &str = "/sbin/ferrix-install --yes --slots /dev/vde\n\
echo \"nazuna: install exited $?\"\n\
exit 0\n";
/// What the install boot must print.
const INSTALLED: &str = "nazuna: install exited 0";
/// How long the install boot may take.
const INSTALL_SECONDS: u64 = 900;

const MIB: u64 = 1 << 20;
const SECTOR: u64 = 512;

const LINUX_LVM: Guid = Guid::from_fields(0xe6d6_d379, 0xf507, 0x44c2, 0xa23c, 0x238f_2a3d_f928);
const MICROSOFT_RESERVED: Guid =
    Guid::from_fields(0xe3c9_e316, 0x0b5c, 0x4db8, 0x817d, 0xf92d_f002_15ae);
const BASIC_DATA: Guid = Guid::from_fields(0xebd0_a0a2, 0xb9e5, 0x4433, 0x87c0, 0x68b6_b726_99c7);
const WINDOWS_RECOVERY: Guid =
    Guid::from_fields(0xde94_bba4, 0x06d1, 0x4d40, 0xa16a, 0xbfd5_0179_d6ac);

/// One of nazuna's NVMe drives.
struct Drive {
    /// The image's file name in the build directory.
    file: &'static str,
    /// QEMU's id for its drive and controller.
    id: &'static str,
    serial: &'static str,
    /// What the drive is. Only said: QEMU's NVMe controller has no property
    /// for the model it reports, which stays `QEMU NVMe Ctrl`, `1b36:0010`.
    model: &'static str,
    /// Its size in sectors, as Linux reports it.
    sectors: u64,
    /// Its partitions, `(type, MiB, name)`, the last taking the rest.
    parts: &'static [(Guid, u64, &'static str)],
}

/// The SN720, as `lsblk` showed it on 2026-10-12: swap, then Ferrix's A/B
/// layout (`docs/AB-BOOT.md`).
const SN720: Drive = Drive {
    file: "nazuna-sn720.img",
    id: "nazuna-sn720",
    serial: "nazuna-sn720",
    model: "WDC PC SN720 SDAPNTW-256G-1006",
    sectors: 500_118_192,
    parts: &[
        (LINUX_SWAP, 40_960, "nazuna-swap"),
        (ESP, 512, "ferrix-esp"),
        (CONTROL_TYPE, 1, "ferrix-bootctl"),
        (SLOT_TYPE, 4_096, "ferrix-slot-a"),
        (SLOT_TYPE, 4_096, "ferrix-slot-b"),
        (LINUX_FILESYSTEM, 0, "ferrix-root"),
    ],
};

/// The Lexar NM790 Ubuntu runs from: its ESP, `/boot` and the LVM volume.
const NM790: Drive = Drive {
    file: "nazuna-nm790.img",
    id: "nazuna-nm790",
    serial: "nazuna-nm790",
    model: "Lexar SSD NM790 2TB",
    sectors: 3_907_029_168,
    parts: &[
        (ESP, 1_024, ""),
        (LINUX_FILESYSTEM, 2_048, ""),
        (LINUX_LVM, 0, ""),
    ],
};

/// The Lexar NQ790 Windows runs from.
const NQ790: Drive = Drive {
    file: "nazuna-nq790.img",
    id: "nazuna-nq790",
    serial: "nazuna-nq790",
    model: "Lexar SSD NQ790 1TB",
    sectors: 1_953_525_168,
    parts: &[
        (ESP, 100, "Basic data partition"),
        (MICROSOFT_RESERVED, 16, "Microsoft reserved partition"),
        (BASIC_DATA, 952_832, "Basic data partition"),
        (WINDOWS_RECOVERY, 0, ""),
    ],
};

/// `cargo xtask run-nazuna`: install, then boot. Watched in this terminal,
/// or with `--timeout` headless for that long, its serial log kept.
pub(crate) fn run(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new("run-nazuna: nazuna is an x86-64 PC"));
    }
    if !cfg!(target_os = "linux") {
        return Err(Error::new(
            "run-nazuna runs on a Linux host: its disks are sparse files of the drives' real \
             sizes (2 TB and more), and the processor is nazuna's own under KVM, so run it on \
             nazuna (docs/NAZUNA-VM.md)",
        ));
    }

    let carried = files(arch)?.ok_or_else(|| Error::new("ferrix-install is not built"))?;
    let natives = native::build(arch, args.release)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let initramfs = initramfs::build(None, &natives, None, &carried)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let zinc = zinc::built(arch)?.ok_or_else(|| Error::new("zinc is not built for x86_64"))?;
    let build = paths::build_dir(arch);

    let cmdline = live_cmdline(args);
    let live = build.join("nazuna-live.img");
    write_medium_with(arch, &loader, &kernel, &initramfs, &live, &cmdline)?;

    let sn720 = build.join(SN720.file);
    make(&sn720, &SN720)?;
    swap_signature(&sn720)?;
    for drive in [&NM790, &NQ790] {
        let path = build.join(drive.file);
        if !path.exists() {
            make(&path, drive)?;
        }
    }

    // The install, as the customer's was: from the live medium, onto the
    // SN720's layout. The test machine's own devices are there for this boot.
    let script = cargo::build_kernel_with_init(arch, args.release, &zinc, INSTALL)?;
    let image = fat::write_image_with(
        arch,
        &loader,
        &script,
        &initramfs,
        Some("ferrix.root=tmpfs"),
    )?;
    let mut installing = args.clone();
    installing.timeout = INSTALL_SECONDS;
    installing.install_disks = Some((live, sn720.clone()));
    let lines = qemu::watch_lines(arch, &image, &script, &installing, crate::shell::EXITED)?;
    if !lines.iter().any(|line| line.contains(INSTALLED)) {
        return Err(Error::new(
            "run-nazuna: ferrix-install did not install the SN720's copy; its lines are in the \
             serial log",
        ));
    }
    println!("  {arch}: Ferrix installed on the SN720's copy, `{cmdline}` carried");

    let mut machine = args.clone();
    machine.nazuna = true;
    if !args.timeout_given {
        return qemu::run(arch, &sn720, &machine);
    }
    let (panicked, _, log) = qemu::observe(arch, &sn720, &kernel, &machine)?;
    println!(
        "  {arch}: nazuna {} after {}s; serial log {}",
        if panicked { "panicked" } else { "still running" },
        args.timeout,
        log.display()
    );
    Ok(())
}

/// The live medium's command line, with `--kernel-option`'s words after it.
fn live_cmdline(args: &Args) -> String {
    let mut words = vec![LIVE_CMDLINE.to_owned()];
    words.extend(args.kernel_options.iter().cloned());
    words.join(" ")
}

/// A fresh image of `drive`: its size, sparse, with its partition table.
fn make(path: &Path, drive: &Drive) -> Result<()> {
    let _ = lay(path, drive.sectors * SECTOR, drive.parts)?;
    println!(
        "  {} as {} ({} GB, sparse)",
        drive.model,
        path.display(),
        drive.sectors * SECTOR / 1_000_000_000
    );
    Ok(())
}

/// A swap signature at the start of the SN720's first partition, as Linux's
/// `mkswap` leaves, which the installer must leave alone.
fn swap_signature(path: &Path) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let _ = file.seek(SeekFrom::Start(MIB + 4086))?;
    file.write_all(b"SWAPSPACE2")?;
    file.sync_all()?;
    Ok(())
}

/// `-m` and `-smp`: 16 GiB and nazuna's 12 cores of two threads, unless
/// `--memory` or `--smp` say otherwise.
pub(crate) fn memory_and_processors(args: &Args) -> (String, String) {
    let memory = if args.memory_given {
        args.memory.to_string()
    } else {
        "16384".to_owned()
    };
    let smp = if args.smp_given {
        args.smp.to_string()
    } else {
        "24,sockets=1,cores=12,threads=2".to_owned()
    };
    (memory, smp)
}

/// The processor: nazuna's own under KVM (which is nazuna, since nothing
/// else runs this), the model every boot has otherwise.
pub(crate) fn cpu(binary: &Path, accelerator: &str) -> String {
    if accelerator == "kvm" {
        "host,migratable=off,+invtsc".to_owned()
    } else {
        qemu::x86_cpu_for(binary, accelerator)
    }
}

/// The drives and USB. `sn720` is the image booted from.
pub(crate) fn attach(command: &mut Command, sn720: &Path) {
    let build = paths::build_dir(Arch::X86_64);
    let drive = |command: &mut Command, drive: &Drive, path: &Path, bus: &str, boot: bool| {
        let _ = command.args([
            "-drive",
            &format!(
                "file={},if=none,format=raw,id={},cache=writeback",
                path.display(),
                drive.id
            ),
            "-device",
            &format!(
                "nvme,drive={id},serial={},bus={bus}{}",
                drive.serial,
                if boot { ",bootindex=0" } else { "" },
                id = drive.id,
            ),
        ]);
    };
    println!(
        "  x86_64: nazuna's shape: the NM790 on a root port, the SN720 ({}) and the NQ790 below \
         a switch, booted from the SN720; an AMD IOMMU; xHCI with a keyboard and a mouse",
        sn720.display()
    );
    let _ = command.args([
        "-device",
        "pcie-root-port,id=nazuna.cpu,chassis=10,slot=10",
        "-device",
        "pcie-root-port,id=nazuna.chipset,chassis=11,slot=11",
        "-device",
        "x3130-upstream,id=nazuna.up,bus=nazuna.chipset",
        "-device",
        "xio3130-downstream,id=nazuna.m2-3,bus=nazuna.up,chassis=12,slot=12",
        "-device",
        "xio3130-downstream,id=nazuna.m2-4,bus=nazuna.up,chassis=13,slot=13",
    ]);
    drive(command, &NM790, &build.join(NM790.file), "nazuna.cpu", false);
    drive(command, &SN720, sn720, "nazuna.m2-3", true);
    drive(command, &NQ790, &build.join(NQ790.file), "nazuna.m2-4", false);
    let _ = command.args([
        "-device",
        "qemu-xhci,id=nazuna.xhci",
        "-device",
        "usb-kbd,bus=nazuna.xhci.0",
        "-device",
        "usb-mouse,bus=nazuna.xhci.0",
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sn720_is_laid_out_as_lsblk_showed_it() {
        // Start sectors and lengths of nvme1n1p1..p5 on 2026-10-12; p6 ran
        // to the last usable sector, `lay` ends it on the MiB below.
        let real = [
            (2_048, 83_886_080),
            (83_888_128, 1_048_576),
            (84_936_704, 2_048),
            (84_938_752, 8_388_608),
            (93_327_360, 8_388_608),
        ];
        let mut at = MIB / SECTOR;
        for (&(_, mib, _), (start, length)) in SN720.parts.iter().zip(real) {
            assert_eq!((at, mib * MIB / SECTOR), (start, length));
            at += mib * MIB / SECTOR;
        }
        assert_eq!(at, 101_715_968, "the root's first sector");
    }

    #[test]
    fn the_command_line_carries_the_options_after_its_own() {
        let args = Args {
            kernel_options: vec!["ferrix.breadcrumbs".to_owned()],
            ..Args::default()
        };
        assert_eq!(
            live_cmdline(&args),
            "ferrix.root=tmpfs ferrix.fbcon ferrix.breadcrumbs"
        );
    }

    #[test]
    fn nazunas_processors_unless_asked() {
        let (memory, smp) = memory_and_processors(&Args::default());
        assert_eq!(memory, "16384");
        assert_eq!(smp, "24,sockets=1,cores=12,threads=2");
    }
}
