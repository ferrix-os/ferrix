//! `test-install --slots`: the A/B layout (`docs/AB-BOOT.md`) from the
//! running system's side -- `ferrix-install --slots` and `bootctl` -- on
//! virtual disks laid out as the customer's SN720 was on 2026-10-11, scaled
//! down. x86-64, one QEMU process per boot, so each boot is a power cycle.
//!
//! The disks, made on the host:
//!
//! * `ab-target.img`, 3 GiB, blank: the installer gives it the layout.
//! * `ab-premade.img`, 3 GiB, with the layout already (Ferrix's types) and a
//!   first partition standing for the host's swap, filled with a pattern:
//!   the installer must reuse it, writing neither its table nor its swap.
//! * `ab-sn720.img`: the SN720 as partitioned -- the names, but Linux's
//!   data type on the control partition and the slots: the installer and
//!   `bootctl` must both refuse it, and it must hash the same afterwards.
//!
//! The boots:
//!
//! 1. The installer, from the live medium: `--slots` on the blank disk, on
//!    the premade one, and on the SN720 copy (refused). Then `bootctl`
//!    refuses to act with two disks carrying a control partition, and
//!    refuses `try` on a boot that did not come from a slot.
//! 2. The installed disk alone through OVMF: it must put `/` on its root
//!    partition, `vdd6`. (Until ab-boot's loader lands this boots the ESP's
//!    rescue files; afterwards slot A.)
//! 3. `bootctl` as slot A, good: `status`, `try` of a new slot image into
//!    B, and a refusal of the SN720 copy. Then the host takes one try off
//!    the record, as the loader does, with the loader's own crate.
//! 4. As slot B on trial: `try` refused (the idle slot would be the good
//!    one), `good` marks B.
//! 5. As slot B, good: `try` of the live medium's FAT image into A,
//!    `verify a`, `rollback`. Then the host flips one byte of slot A's
//!    initramfs.
//! 6. As slot A with a record that says B is good: `verify a` refuses the
//!    corrupted slot, and `try` refuses to write (the idle slot would be
//!    the running one).
//!
//! 7. End to end, through the loader: a system whose shell runs `bootctl`
//!    is installed on a fresh disk and booted from it alone: slot A, good,
//!    writes and arms slot B with `bootctl try`; at the next reset the
//!    loader starts B's trial and `bootctl good` keeps it; at the next, B
//!    boots as good.
//!
//! Throughout: the target's swap pattern, the premade disk's swap and
//! table, and the SN720 copy hash the same at the end as before.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ferrix_partition::{
    ESP, Guid, LINUX_FILESYSTEM, Partition, parse_entries, parse_header, write_table,
};
use ferrix_slots::record::{COPY_OFFSETS, Current, select, write_copy};
use ferrix_slots::slot::{HEADER_LEN, SlotHeader};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, sha256, zinc};

use super::{SECTOR, files, write_medium};

const SLOT_TYPE: Guid = Guid::from_fields(0x5475_b681, 0x82f5, 0x47da, 0xa5a1, 0x55db_0e7e_2bd3);
const CONTROL_TYPE: Guid = Guid::from_fields(0xc29a_a8c5, 0xc19c, 0x4147, 0x838f, 0x01ac_cca2_8be4);
const LINUX_SWAP: Guid = Guid::from_fields(0x0657_fd6d, 0xa4ab, 0x43c4, 0x84e5, 0x0933_c84b_4f4f);
const MIB: u64 = 1 << 20;
const MIB_SECTORS: u64 = MIB / SECTOR;
const DISK_BYTES: u64 = 3 << 30;
/// The scaled layout: swap, ESP (the live volume's 512 MiB), control,
/// two slots, root the rest.
const SWAP_MIB: u64 = 16;
const ESP_MIB: u64 = 512;
const SLOT_MIB: u64 = 600;
/// How long each boot may take, unless `--timeout` says.
const BOOT_SECONDS: u64 = 900;

/// The installer boot's script.
const INSTALL: &str = "/sbin/ferrix-install --yes --slots --swap-mib 16 --esp-mib 512 --slot-mib 600 /dev/vde\n\
echo \"slots: blank exited $?\"\n\
/sbin/ferrix-install --yes --slots /dev/vdf\n\
echo \"slots: premade exited $?\"\n\
/sbin/ferrix-install --yes --slots /dev/vdg\n\
echo \"slots: sn720 exited $?\"\n\
/sbin/bootctl status\n\
echo \"bootctl: two disks exited $?\"\n\
/sbin/bootctl --disk /dev/vdf try /dev/vdd1\n\
echo \"bootctl: rescue try exited $?\"\n\
exit 0\n";

/// Boot 3: slot A, good.
const AS_A: &str = "/sbin/bootctl status\n\
echo \"bootctl: status exited $?\"\n\
/sbin/bootctl good\n\
echo \"bootctl: good on a good slot exited $?\"\n\
/sbin/bootctl try /dev/vdd\n\
echo \"bootctl: try exited $?\"\n\
/sbin/bootctl status\n\
/sbin/bootctl --disk /dev/vdf status\n\
echo \"bootctl: sn720 exited $?\"\n\
exit 0\n";

/// Boot 4: slot B on trial.
const AS_TRIAL_B: &str = "/sbin/bootctl try /dev/vdd\n\
echo \"bootctl: try on trial exited $?\"\n\
/sbin/bootctl good\n\
echo \"bootctl: good exited $?\"\n\
/sbin/bootctl status\n\
exit 0\n";

/// Boot 5: slot B, good.
const AS_B: &str = "/sbin/bootctl try /dev/vdg\n\
echo \"bootctl: try fat exited $?\"\n\
/sbin/bootctl verify a\n\
echo \"bootctl: verify exited $?\"\n\
/sbin/bootctl rollback\n\
echo \"bootctl: rollback exited $?\"\n\
/sbin/bootctl status\n\
exit 0\n";

/// Boot 6: slot A, which the record does not say is good.
const AS_STALE_A: &str = "/sbin/bootctl verify a\n\
echo \"bootctl: verify corrupted exited $?\"\n\
/sbin/bootctl status\n\
/sbin/bootctl try /dev/vdd\n\
echo \"bootctl: try stale exited $?\"\n\
exit 0\n";

/// The end-to-end system's script: the same system in both slots, doing
/// what a person does -- on slot A, good, `bootctl try` the new image; on
/// the trial of B, `bootctl good`.
const E2E: &str = "read line < /proc/cmdline\n\
echo \"e2e: booted with $line\"\n\
case \"$line\" in\n\
*ferrix.slot.boot=trial*) /sbin/bootctl good; echo \"e2e: good exited $?\" ;;\n\
*ferrix.slot=a*) /sbin/bootctl try /dev/vde; echo \"e2e: try exited $?\" ;;\n\
esac\n\
/sbin/bootctl status\n\
exit 0\n";

/// The end-to-end install, from the end-to-end system's live medium.
const E2E_INSTALL: &str = "/sbin/ferrix-install --yes --slots --esp-mib 512 --slot-mib 600 /dev/vde\n\
echo \"slots: e2e exited $?\"\n\
exit 0\n";

/// What boot 1 must print.
const INSTALL_EXPECT: &[(&str, &str)] = &[
    (
        "slots: blank exited 0",
        "the installer did not make the layout on the blank disk",
    ),
    (
        "will be given the A/B layout",
        "the installer did not say it makes the layout",
    ),
    ("slot a verified", "the installer did not verify slot a"),
    (
        "slots: premade exited 0",
        "the installer did not install on the premade layout",
    ),
    (
        "has the A/B layout and keeps its partition table",
        "the installer did not reuse the premade layout",
    ),
    (
        "slots: sn720 exited 1",
        "the installer did not refuse the SN720 as partitioned",
    ),
    (
        "not Ferrix's boot control type",
        "the installer's refusal of the SN720 does not say why",
    ),
    (
        "bootctl: two disks exited 1",
        "bootctl acted with two disks carrying a control partition",
    ),
    (
        "more than one disk has a partition of Ferrix's boot control type",
        "bootctl's two-disk refusal does not say why",
    ),
    (
        "bootctl: rescue try exited 1",
        "bootctl try ran on a boot that did not come from a slot",
    ),
    (
        "did not boot from a slot",
        "bootctl's rescue-boot refusal does not say why",
    ),
];

/// What boot 3 must print.
const AS_A_EXPECT: &[(&str, &str)] = &[
    ("bootctl: status exited 0", "bootctl status failed"),
    (
        "verifies: ferrix installed",
        "slot a, as installed, does not verify",
    ),
    (
        "bootctl: slot a is already good",
        "bootctl good on a good slot did not say so",
    ),
    (
        "bootctl: slot b read back and verified",
        "bootctl try did not verify slot b",
    ),
    ("bootctl: try exited 0", "bootctl try failed"),
    ("trial:   b, 1 tries left", "bootctl try did not arm slot b"),
    (
        "bootctl: sn720 exited 1",
        "bootctl did not refuse the SN720 as partitioned",
    ),
    (
        "not Ferrix's boot control type",
        "bootctl's refusal of the SN720 does not say why",
    ),
];

/// What boot 4 must print.
const AS_TRIAL_B_EXPECT: &[(&str, &str)] = &[
    (
        "bootctl: try on trial exited 1",
        "bootctl try wrote a slot while running a trial",
    ),
    (
        "runs slot b on trial",
        "bootctl's refusal on trial does not say why",
    ),
    ("bootctl: good exited 0", "bootctl good failed on the trial"),
    ("good:    b", "the record does not say b is good"),
];

/// What boot 5 must print.
const AS_B_EXPECT: &[(&str, &str)] = &[
    (
        "bootctl: try fat exited 0",
        "bootctl try of a FAT image failed",
    ),
    (
        "writing the FAT image /dev/vdg to slot a",
        "bootctl did not take the FAT image",
    ),
    (
        "bootctl: verify exited 0",
        "slot a does not verify after try",
    ),
    ("bootctl: rollback exited 0", "bootctl rollback failed"),
    ("trial:   none", "rollback left a trial armed"),
];

/// What boot 6 must print.
const AS_STALE_A_EXPECT: &[(&str, &str)] = &[
    (
        "bootctl: verify corrupted exited 1",
        "bootctl verify passed a corrupted slot",
    ),
    (
        "its initramfs does not match the header's SHA-256",
        "the corrupted slot's refusal does not say why",
    ),
    (
        "bootctl: try stale exited 1",
        "bootctl try wrote while booted from a slot the record does not call good",
    ),
    (
        "but the record says b is good",
        "the stale refusal does not say why",
    ),
];

/// `test-install --slots`: see the module documentation.
pub(super) fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-install --slots runs on x86-64 only for now",
        ));
    }
    let gate = Gate::prepare(args)?;
    let premade_kept = hash_ranges(&gate.premade, &premade_untouched(&gate.premade)?)?;
    let sn720_whole = [(0, file_len(&gate.sn720)?)];
    let sn720_hash = hash_ranges(&gate.sn720, &sn720_whole)?;

    // 1. The installer.
    let lines = gate.run(
        INSTALL,
        "ferrix.root=tmpfs",
        (gate.live.clone(), gate.target.clone()),
        vec![gate.premade.clone(), gate.sn720.clone()],
    )?;
    require(&lines, INSTALL_EXPECT)?;
    let premade_now = premade_untouched(&gate.premade)?;
    same(
        &gate.premade,
        &premade_now,
        &premade_kept,
        "the premade disk's swap or table",
    )?;
    same(&gate.sn720, &sn720_whole, &sn720_hash, "the SN720 copy")?;
    println!("  {arch}: the installer made the layout, reused it, and refused the SN720 as it is");

    // The target's swap gets a pattern; it must hash the same at the end.
    let swap = partition(&gate.target, 1)?;
    fill(&gate.target, swap)?;
    let swap_hash = hash_ranges(&gate.target, &[swap])?;

    // 2. The installed disk alone.
    gate.installed()?;
    gate.bootctl_boots()?;
    gate.end_to_end()?;

    same(
        &gate.target,
        &[swap],
        &swap_hash,
        "the target's swap partition",
    )?;
    same(
        &gate.premade,
        &premade_now,
        &premade_kept,
        "the premade disk's swap or table",
    )?;
    same(&gate.sn720, &sn720_whole, &sn720_hash, "the SN720 copy")?;
    println!(
        "  {arch}: swap partitions and the SN720 copy hash as before; test-install --slots passed"
    );
    Ok(())
}

/// What the gate's boots share.
struct Gate<'a> {
    args: &'a Args,
    loader: PathBuf,
    kernel: cargo::Kernel,
    initramfs: Vec<u8>,
    zinc: PathBuf,
    live: PathBuf,
    target: PathBuf,
    premade: PathBuf,
    sn720: PathBuf,
}

impl<'a> Gate<'a> {
    /// Build what the boots need and make the disks.
    fn prepare(args: &'a Args) -> Result<Gate<'a>> {
        let arch = Arch::X86_64;
        let carried = files(arch)?.ok_or_else(|| Error::new("ferrix-install is not built"))?;
        let natives = native::build(arch, args.release)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let kernel = cargo::build_kernel(arch, args.release)?;
        let build = paths::build_dir(arch);
        let live = build.join("install-live.img");
        write_medium(arch, &loader, &kernel, &initramfs, &live)?;
        let zinc = zinc::built(arch)?.ok_or_else(|| Error::new("zinc is not built for x86_64"))?;
        let gate = Gate {
            args,
            loader,
            kernel,
            initramfs,
            zinc,
            live,
            target: build.join("ab-target.img"),
            premade: build.join("ab-premade.img"),
            sn720: build.join("ab-sn720.img"),
        };
        let _ = fresh(&gate.target, DISK_BYTES)?;
        make_premade(&gate.premade)?;
        make_sn720(&gate.sn720)?;
        Ok(gate)
    }

    /// Boot a kernel whose shell runs `script`, with `cmdline`, the two
    /// disks as `vdd` and `vde` and `extra` after them; its lines.
    fn run(
        &self,
        script: &str,
        cmdline: &str,
        disks: (PathBuf, PathBuf),
        extra: Vec<PathBuf>,
    ) -> Result<Vec<String>> {
        let arch = Arch::X86_64;
        let scripted = cargo::build_kernel_with_init(arch, self.args.release, &self.zinc, script)?;
        let image = fat::write_image_with(
            arch,
            &self.loader,
            &scripted,
            &self.initramfs,
            Some(cmdline),
        )?;
        let mut booted = self.args.clone();
        if !booted.timeout_given {
            // Each boot copies hundreds of MiB through a ring-3 disk driver.
            booted.timeout = BOOT_SECONDS;
        }
        booted.install_disks = Some(disks);
        booted.extra_disks = extra;
        qemu::watch_lines(arch, &image, &scripted, &booted, crate::shell::EXITED)
    }

    /// Boot 2: the installed disk alone, through OVMF.
    fn installed(&self) -> Result<()> {
        let mut installed = self.args.clone();
        installed.boot_virtio = true;
        if !installed.timeout_given {
            installed.timeout = BOOT_SECONDS;
        }
        let lines = qemu::test_boot_lines(Arch::X86_64, &self.target, &self.kernel, &installed)?;
        require(
            &lines,
            &[(
                "/ is btrfs on vdd6; the system was installed on it",
                "the installed disk did not put / on its root partition",
            )],
        )?;
        println!("  x86_64: the installed disk booted with / on vdd6");
        Ok(())
    }

    /// Boots 7 to 10, end to end through the loader (`docs/AB-BOOT.md`):
    /// install a system whose shell runs [`E2E`], boot it from the disk alone
    /// (slot A, good), where it writes and arms slot B with `bootctl try`;
    /// reset into B's trial, where `bootctl good` keeps it; reset into B,
    /// good.
    fn end_to_end(&self) -> Result<()> {
        let arch = Arch::X86_64;
        let build = paths::build_dir(arch);
        let system = cargo::build_kernel_with_init(arch, self.args.release, &self.zinc, E2E)?;
        let medium = build.join("ab-e2e-live.img");
        write_medium(arch, &self.loader, &system, &self.initramfs, &medium)?;
        let disk = build.join("ab-e2e.img");
        let _ = fresh(&disk, DISK_BYTES)?;
        let lines = self.run(
            E2E_INSTALL,
            "ferrix.root=tmpfs",
            (medium, disk.clone()),
            Vec::new(),
        )?;
        require(
            &lines,
            &[("slots: e2e exited 0", "the end-to-end install failed")],
        )?;
        // The new system: this one again, as `cargo xtask slot-image` writes it.
        let image = build.join("ab-e2e-new.img");
        let kernel_bytes = std::fs::read(&system.elf)?;
        let archive = system.initramfs(&self.initramfs)?;
        let label = "ferrix e2e new";
        crate::slot_image::write(&image, label, &kernel_bytes, &archive, b"ferrix.root=btrfs")?;

        let mut booted = self.args.clone();
        booted.boot_virtio = true;
        booted.extra_disks = vec![image];
        if !booted.timeout_given {
            booted.timeout = BOOT_SECONDS;
        }
        let boot = || qemu::watch_lines(arch, &disk, &system, &booted, crate::shell::EXITED);
        require(
            &boot()?,
            &[
                (
                    "boot     slot a good seq 1",
                    "the loader did not boot slot A as good",
                ),
                (
                    "bootctl: slot b read back and verified",
                    "bootctl try did not write slot B",
                ),
                (
                    "e2e: try exited 0",
                    "bootctl try failed on the installed system",
                ),
                ("trial:   b, 1 tries left", "bootctl try did not arm slot B"),
            ],
        )?;
        println!("  {arch}: installed, booted slot a through the loader, wrote and armed b");
        require(
            &boot()?,
            &[
                (
                    "boot     slot b trial seq 3",
                    "the loader did not start B's trial after the reset",
                ),
                ("e2e: good exited 0", "bootctl good failed on B's trial"),
                ("good:    b", "the record does not say b is good"),
            ],
        )?;
        println!("  {arch}: reset into b's trial, and bootctl good kept it");
        require(
            &boot()?,
            &[
                (
                    "boot     slot b good seq 4",
                    "the loader did not boot B as good after the reset",
                ),
                (
                    "booted:  b as good, under record 4",
                    "bootctl status does not say B booted as good",
                ),
            ],
        )?;
        println!("  {arch}: reset into b, good: end to end through the loader");
        Ok(())
    }

    /// Boots 3 to 6: `bootctl` on the installed disk, with the loader's
    /// words on the command line as a slot boot has them.
    fn bootctl_boots(&self) -> Result<()> {
        let build = paths::build_dir(Arch::X86_64);
        // A new system to try: the live system as a slot image, as `cargo
        // xtask slot-image` writes one, and as an image's FAT volume.
        let image = build.join("ab-new-slot.img");
        let kernel_bytes = std::fs::read(&self.kernel.elf)?;
        let archive = self.kernel.initramfs(&self.initramfs)?;
        let label = "ferrix test-install new";
        crate::slot_image::write(&image, label, &kernel_bytes, &archive, b"ferrix.root=btrfs")?;
        let fat_image = build.join("ab-fat.img");
        let written = fat::write_image_with(
            Arch::X86_64,
            &self.loader,
            &self.kernel,
            &self.initramfs,
            Some("ferrix.root=btrfs"),
        )?;
        let _ = std::fs::copy(written, &fat_image)?;
        // The loader names the disk it booted from by its GUID.
        let disk = format!(
            "ferrix.slot.disk={} ferrix.root=tmpfs",
            disk_guid(&self.target)?
        );
        let boot = |script: &str, words: String| {
            let extra = vec![self.sn720.clone(), fat_image.clone()];
            self.run(
                script,
                &format!("{words} {disk}"),
                (image.clone(), self.target.clone()),
                extra,
            )
        };

        // 3. Slot A, good.
        let a_good = "ferrix.slot=a ferrix.slot.boot=good ferrix.slot.seq=1";
        require(&boot(AS_A, a_good.to_owned())?, AS_A_EXPECT)?;
        let sequence = loader_takes_a_try(&self.target)?;
        println!(
            "  x86_64: slot b written and armed; the loader's decrement made record {sequence}"
        );

        // 4. Slot B on trial.
        let words = format!("ferrix.slot=b ferrix.slot.boot=trial ferrix.slot.seq={sequence}");
        require(&boot(AS_TRIAL_B, words)?, AS_TRIAL_B_EXPECT)?;
        println!("  x86_64: on trial, try was refused and good marked slot b");

        // 5. Slot B, good.
        let words = format!(
            "ferrix.slot=b ferrix.slot.boot=good ferrix.slot.seq={}",
            sequence + 1
        );
        require(&boot(AS_B, words)?, AS_B_EXPECT)?;
        let at = flip_initramfs_byte(&self.target, 0)?;
        println!(
            "  x86_64: slot a written from a FAT image and taken back; byte {at} of it flipped"
        );

        // 6. Slot A, which the record no longer calls good.
        require(&boot(AS_STALE_A, a_good.to_owned())?, AS_STALE_A_EXPECT)
    }
}

fn require(lines: &[String], wanted: &[(&str, &str)]) -> Result<()> {
    for (line, problem) in wanted {
        if !lines.iter().any(|l| l.contains(line)) {
            return Err(Error::new(format!(
                "x86_64: {problem} (no `{line}`); its lines are in the serial log"
            )));
        }
    }
    Ok(())
}

fn fresh(path: &Path, bytes: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    file.set_len(bytes)?;
    Ok(file)
}

fn file_len(path: &Path) -> Result<u64> {
    Ok(std::fs::metadata(path)?.len())
}

/// Write a disk of `bytes` with these partitions, each `(type, MiB, name)`
/// from 1 MiB on, the last taking the rest; returns their byte ranges.
fn lay(path: &Path, bytes: u64, parts: &[(Guid, u64, &str)]) -> Result<Vec<(u64, u64)>> {
    let mut file = fresh(path, bytes)?;
    let sectors = bytes / SECTOR;
    let (_, last_usable) =
        ferrix_partition::usable(sectors, SECTOR as u32).map_err(|e| Error::new(e.to_string()))?;
    let mut at = MIB_SECTORS;
    let mut out = Vec::new();
    let mut entries = Vec::new();
    for (index, &(kind, mib, name)) in parts.iter().enumerate() {
        let last = if index + 1 == parts.len() {
            (last_usable + 1) / MIB_SECTORS * MIB_SECTORS - 1
        } else {
            at + mib * MIB_SECTORS - 1
        };
        let mut unique = [0x5A_u8; 16];
        unique[0] = index as u8;
        entries.push(Partition::new(kind, Guid::random(unique), at, last, name));
        out.push((at * SECTOR, (last + 1 - at) * SECTOR));
        at = last + 1;
    }
    let table = write_table(sectors, SECTOR as u32, Guid::random([0xA5; 16]), &entries)
        .map_err(|e| Error::new(e.to_string()))?;
    for (lba, sector) in table {
        let _ = file.seek(SeekFrom::Start(lba * SECTOR))?;
        file.write_all(&sector)?;
    }
    Ok(out)
}

/// The SN720's names, for a layout of these types.
fn sn720_parts(
    control: Guid,
    slot: Guid,
    esp_mib: u64,
    slot_mib: u64,
) -> [(Guid, u64, &'static str); 6] {
    [
        (LINUX_SWAP, SWAP_MIB, "nazuna-swap"),
        (ESP, esp_mib, "ferrix-esp"),
        (control, 1, "ferrix-bootctl"),
        (slot, slot_mib, "ferrix-slot-a"),
        (slot, slot_mib, "ferrix-slot-b"),
        (LINUX_FILESYSTEM, 0, "ferrix-root"),
    ]
}

fn make_premade(path: &Path) -> Result<()> {
    let ranges = lay(
        path,
        DISK_BYTES,
        &sn720_parts(CONTROL_TYPE, SLOT_TYPE, ESP_MIB, SLOT_MIB),
    )?;
    let swap = *ranges
        .first()
        .ok_or_else(|| Error::new("a layout with no partitions"))?;
    fill(path, swap)
}

fn make_sn720(path: &Path) -> Result<()> {
    let ranges = lay(
        path,
        256 * MIB,
        &sn720_parts(LINUX_FILESYSTEM, LINUX_FILESYSTEM, 8, 64),
    )?;
    let swap = *ranges
        .first()
        .ok_or_else(|| Error::new("a layout with no partitions"))?;
    fill(path, swap)
}

/// The premade disk's ranges the installer must not write: its swap, and
/// its primary and backup tables.
fn premade_untouched(path: &Path) -> Result<Vec<(u64, u64)>> {
    let len = file_len(path)?;
    Ok(vec![
        partition(path, 1)?,
        (0, 34 * SECTOR),
        (len.saturating_sub(33 * SECTOR), 33 * SECTOR),
    ])
}

/// Fill a range with a pattern and a swap signature, as a live swap
/// partition looks.
fn fill(path: &Path, (at, len): (u64, u64)) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(path)?;
    let mut chunk: Vec<u8> = (0..MIB).map(|i| (i * 7 + 3) as u8).collect();
    drop(chunk.splice(4086..4096, *b"SWAPSPACE2"));
    let _ = file.seek(SeekFrom::Start(at))?;
    let mut done = 0;
    while done < len {
        let n = (len - done).min(MIB) as usize;
        file.write_all(chunk.get(..n).unwrap_or_default())?;
        done += n as u64;
    }
    file.sync_all()?;
    Ok(())
}

fn hash_ranges(path: &Path, ranges: &[(u64, u64)]) -> Result<Vec<String>> {
    let mut file = File::open(path)?;
    ranges
        .iter()
        .map(|&(at, len)| {
            let mut bytes =
                vec![0_u8; usize::try_from(len).map_err(|_| Error::new("a range past memory"))?];
            let _ = file.seek(SeekFrom::Start(at))?;
            file.read_exact(&mut bytes)?;
            Ok(sha256::hex(&sha256::digest(&bytes)))
        })
        .collect()
}

fn same(path: &Path, ranges: &[(u64, u64)], before: &[String], what: &str) -> Result<()> {
    if hash_ranges(path, ranges)? != before {
        return Err(Error::new(format!(
            "x86_64: {what} changed ({})",
            path.display()
        )));
    }
    Ok(())
}

/// The GUID of the disk at `path`, as GPT tools print it.
fn disk_guid(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut sector = [0_u8; SECTOR as usize];
    let _ = file.seek(SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut sector)?;
    let header = parse_header(&sector, file_len(path)? / SECTOR)
        .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    Ok(header.disk.to_string())
}

/// The byte range of partition `number` (1-based) of the disk at `path`.
fn partition(path: &Path, number: u32) -> Result<(u64, u64)> {
    let mut file = File::open(path)?;
    let sectors = file_len(path)? / SECTOR;
    let mut sector = [0_u8; SECTOR as usize];
    let _ = file.seek(SeekFrom::Start(SECTOR))?;
    file.read_exact(&mut sector)?;
    let header = parse_header(&sector, sectors)
        .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    let mut array = vec![0_u8; header.entries_bytes()];
    let _ = file.seek(SeekFrom::Start(header.entries_at * SECTOR))?;
    file.read_exact(&mut array)?;
    let entries = parse_entries(&header, &array).map_err(|e| Error::new(e.to_string()))?;
    let (_, part) = entries
        .into_iter()
        .find(|(index, _)| index + 1 == number)
        .ok_or_else(|| Error::new(format!("{} has no partition {number}", path.display())))?;
    Ok((part.first * SECTOR, part.sectors() * SECTOR))
}

/// Take one try off the target's record, as the loader does before it
/// starts a trial slot (`docs/AB-BOOT.md` §4 step 5), with its crate: the
/// next record over the other copy. Returns the new sequence.
fn loader_takes_a_try(path: &Path) -> Result<u64> {
    let (base, _) = partition(path, 3)?;
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut copies = [[0_u8; 512]; 2];
    for (copy, at) in copies.iter_mut().zip(COPY_OFFSETS) {
        let _ = file.seek(SeekFrom::Start(base + at))?;
        file.read_exact(copy)?;
    }
    let current = select(&copies[0], &copies[1]);
    let Current::Valid { record, .. } = current else {
        return Err(Error::new(
            "x86_64: the target's control record is not valid after try",
        ));
    };
    let next = record
        .after_try()
        .ok_or_else(|| Error::new(format!("x86_64: the record has no try to take: {record:?}")))?;
    let copy = COPY_OFFSETS
        .get(write_copy(&current))
        .ok_or_else(|| Error::new("a record copy past the two"))?;
    let _ = file.seek(SeekFrom::Start(base + copy))?;
    file.write_all(&next.encode())?;
    file.sync_all()?;
    Ok(next.sequence)
}

/// Flip one byte in the middle of slot `index`'s (0 = A) initramfs;
/// returns its offset in the slot.
fn flip_initramfs_byte(path: &Path, index: u32) -> Result<u64> {
    let (base, len) = partition(path, 4 + index)?;
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut head = vec![0_u8; HEADER_LEN];
    let _ = file.seek(SeekFrom::Start(base))?;
    file.read_exact(&mut head)?;
    let header = SlotHeader::decode(&head, len)
        .map_err(|why| Error::new(format!("x86_64: slot header before the flip: {why:?}")))?;
    let at = header.initrd.offset + header.initrd.length / 2;
    let mut byte = [0_u8; 1];
    let _ = file.seek(SeekFrom::Start(base + at))?;
    file.read_exact(&mut byte)?;
    byte[0] ^= 0x01;
    let _ = file.seek(SeekFrom::Start(base + at))?;
    file.write_all(&byte)?;
    file.sync_all()?;
    Ok(at)
}
