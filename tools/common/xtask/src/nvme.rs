//! `test-nvme`: the NVMe driver against QEMU's emulated controller
//! (`docs/NVME.md`).
//!
//! The machine has two NVMe drives, each driven by its own `nvme` process:
//!
//! * the btrfs root, 512-byte blocks, below a root port and a PCIe switch
//!   (`qemu::attach_nvme_root`), where a PC's chipset puts an M.2 slot;
//! * a scratch drive of 4096-byte blocks on a root port of its own, every
//!   512 bytes of it `test_disk::sector`'s pattern, so any block's bytes
//!   are known here without reading the image.
//!
//! The gate boots zinc as an interactive shell and types at it. `--boot`
//! names what it requires; `io` when not given:
//!
//! * `io`: both drivers' lines, `/` installed on the NVMe root; blocks of
//!   the scratch drive read back as their pattern -- one block, the last
//!   block, and 128 KiB in one request, which takes a PRP list; pattern
//!   blocks copied onto it with a flush and read back, and found in the
//!   image afterwards with their neighbours untouched; QEMU's own trace
//!   showing Flush commands arrived; then both drivers killed twice while
//!   a loop reads the scratch drive, every read of the loop coming back
//!   right across the kills, a file written and read back on `/` after
//!   each, and host `btrfs check` on the root afterwards.
//! * `fault`: the scratch drive's image behind blkdebug, which fails every
//!   read of one block. That read must fail in the guest, the driver must
//!   say so, and a read beside it must still work.
//! * `bench`: 4 KiB reads of the scratch drive timed by `dd`, in turn with
//!   the same reads of the virtio-blk pattern disk, under whatever load the
//!   host has, which is printed. A measurement: it requires only that the
//!   reads work.
//!
//! * `parts`: a third NVMe drive of 512-byte blocks with a GUID partition
//!   table laid out as the customer's SN720 was (a swap stand-in, the ESP,
//!   the boot control record, two system slots, the root), every sector
//!   outside the table and the record its pattern. The kernel must publish
//!   `nvme<C>n1p1` to `p6` with their sizes, the first and last sector of
//!   each must read back as written, the control partition's first block
//!   must be the record, and `bootctl status` must find the layout by its
//!   partition types and name each node as Linux does.
//!
//! * `up`: the ordinary `test-boot` image and its whole boot, self-checks
//!   and all, with the scratch drive on the bus, and the driver's line for
//!   it required. No shell, so it runs on AArch64 too, where the machine's
//!   IOMMU is an `SMMUv3` and its interrupt controller a GIC.
//!
//! The others are x86-64 only, for the reason `test-restart` is: `dd`, `sha256sum` and
//! `kill` are uutils', built for x86-64 alone (`docs/UUTILS.md` D3).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::restart::{self, Kind};
use crate::{
    Error, Result, btrfs_check, btrfs_disk, cargo, fat, initramfs, native, qemu, sha256, test_disk,
    uutils, zinc,
};

/// How long to wait for the answer to one line.
const PATIENCE: Duration = Duration::from_secs(60);

/// 4096-byte blocks on the scratch drive: 24 MiB, a size no other disk of
/// the machine has, which is how the guest finds it.
const SCRATCH_BLOCKS: u64 = 6144;
/// Bytes in one of them.
const BLOCK: u64 = 4096;
/// The scratch drive's size as sysfs says it, in 512-byte sectors.
const SCRATCH_SECTORS: u64 = SCRATCH_BLOCKS * BLOCK / 512;

/// What ends every answer. Typed with a quote in the middle, so the echo
/// of the typed line never matches it.
const END: &str = "nvme-gate-end";
/// What the disk listing prints before each `name:sectors`.
const DISK_TAG: &str = "nvme-gate-disk=";
/// What a command's exit status follows.
const STATUS_TAG: &str = "nvme-gate-status=";
/// What the load loop prints when it stops.
const LOAD_OVER: &str = "nvme-gate-load-over";
/// The file whose appearance stops the load loop.
const STOP_FILE: &str = "/nvme-gate-stop";

/// The driver's line for a drive that came up.
const UP: &str = "NVMe 1.";
/// The driver's line for completions its watch took before any interrupt.
const LATE_INTERRUPT: &str = "nvme     completions waited 100 ms with no interrupt";
/// The driver's line for a request it failed.
const FAILED: &str = "nvme     request ";

/// What `--boot` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Io,
    Fault,
    Bench,
    Parts,
    Up,
}

impl Mode {
    fn asked(boot: Option<&str>) -> Result<Mode> {
        match boot {
            None | Some("io") => Ok(Mode::Io),
            Some("fault") => Ok(Mode::Fault),
            Some("bench") => Ok(Mode::Bench),
            Some("parts") => Ok(Mode::Parts),
            Some("up") => Ok(Mode::Up),
            Some(other) => Err(Error::new(format!(
                "test-nvme --boot takes io, fault, bench, parts or up, not `{other}`"
            ))),
        }
    }
}

/// The block of the scratch drive blkdebug fails reads of, in `fault`.
const BAD_BLOCK: u64 = 1000;

/// Build the image, boot it with the two drives, and require what `--boot`
/// names.
///
/// # Errors
///
/// When the image cannot be built, the boot fails or panics, or a
/// requirement does not hold.
pub(crate) fn test_nvme(args: &Args) -> Result<()> {
    let mode = Mode::asked(args.boot.as_deref())?;
    if mode == Mode::Up {
        return up(args);
    }
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-nvme's shell runs on x86-64 only: `dd`, `sha256sum` and `kill` come from \
             uutils, which is built for x86-64 alone (docs/UUTILS.md D3); `--boot up` boots \
             the driver on AArch64",
        ));
    }
    let (image, kernel) = shell_image(arch, args, mode == Mode::Parts)?;

    let (booted, scratch, trace) = drives(arch, args, mode)?;
    let checker = match mode {
        Mode::Io => Some(btrfs_check::Checker::required()?),
        Mode::Fault | Mode::Bench | Mode::Parts | Mode::Up => None,
    };
    println!(
        "  {arch}: test-nvme --boot {} (timeout {}s)",
        match mode {
            Mode::Io => "io",
            Mode::Fault => "fault",
            Mode::Bench => "bench",
            Mode::Parts => "parts",
            Mode::Up => "up",
        },
        args.timeout
    );
    let mut failures: Vec<String> = Vec::new();
    let mut said: Vec<String> = Vec::new();
    let lines = qemu::watch_then(
        arch,
        &image,
        &kernel,
        &booted,
        qemu::SUCCESS_MARKER,
        |watching| {
            watching.stop_when_done();
            let outcome = match mode {
                Mode::Io => io(watching),
                Mode::Fault => fault(watching),
                Mode::Parts => parts(watching),
                Mode::Bench | Mode::Up => bench(watching),
            };
            match outcome {
                Ok(lines) => said = lines,
                Err(failure) => failures.push(failure),
            }
            Ok(())
        },
    )?;
    if let Some(panic) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        failures.push(format!("the kernel panicked: {}", panic.trim()));
    }
    if failures.is_empty() {
        let after = match mode {
            Mode::Io => after_io(&lines, &scratch, &trace),
            Mode::Fault => after_fault(&lines),
            Mode::Parts => after_parts(&lines),
            Mode::Bench | Mode::Up => Ok(()),
        };
        if let Err(failure) = after {
            failures.push(failure);
        }
    }
    if failures.is_empty()
        && let Some(checker) = checker
        && let Err(error) = checker.run(&btrfs_disk::test_root_path(arch), arch)
    {
        failures.push(error.to_string());
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: test-nvme failed:\n");
        for failure in &failures {
            message.push_str("    - ");
            message.push_str(failure);
            message.push('\n');
        }
        message.push_str("  The whole transcript is above and in the serial log.");
        return Err(Error::new(message));
    }
    for line in &said {
        println!("  {arch}: {line}");
    }
    Ok(())
}

/// The boot's arguments with the drives `mode` needs written and
/// attached, the scratch drive's image and QEMU's trace file.
fn drives(arch: Arch, args: &Args, mode: Mode) -> Result<(Args, PathBuf, PathBuf)> {
    let directory = paths::build_dir(arch);
    std::fs::create_dir_all(&directory)?;
    let scratch = directory.join("nvme-scratch.img");
    std::fs::write(&scratch, scratch_bytes(0, SCRATCH_BLOCKS))
        .map_err(|error| Error::new(format!("writing {}: {error}", scratch.display())))?;
    let trace = directory.join("nvme-trace.log");
    let _ = std::fs::remove_file(&trace);

    let mut booted = args.clone();
    booted.btrfs_root = true;
    booted.nvme = true;
    booted.nvme_scratch = Some(scratch.clone());
    booted.nvme_trace = Some(trace.clone());
    if mode == Mode::Fault {
        booted.nvme_fault = Some(fault_rules(&directory)?);
    }
    if mode == Mode::Parts {
        let gpt = directory.join("nvme-gpt.img");
        std::fs::write(&gpt, gpt_bytes()?)
            .map_err(|error| Error::new(format!("writing {}: {error}", gpt.display())))?;
        booted.nvme_gpt = Some(gpt);
    }
    Ok((booted, scratch, trace))
}

/// `--boot up`: each architecture's `test-boot`, with the scratch drive.
fn up(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            return Err(Error::new(
                "test-nvme --boot up is for x86-64 and AArch64: ARMv7-A's machine boots \
                 through U-Boot and keeps its virtio disks",
            ));
        }
        let (image, kernel) = crate::build_image(arch, args)?;
        let directory = paths::build_dir(arch);
        std::fs::create_dir_all(&directory)?;
        let scratch = directory.join("nvme-scratch.img");
        std::fs::write(&scratch, scratch_bytes(0, SCRATCH_BLOCKS))
            .map_err(|error| Error::new(format!("writing {}: {error}", scratch.display())))?;
        let mut booted = args.clone();
        booted.nvme_scratch = Some(scratch);
        let lines = qemu::test_boot_lines(arch, &image, &kernel, &booted)?;
        let up = lines
            .iter()
            .find(|line| line.contains("nvme     ") && line.contains(UP))
            .filter(|line| line.contains(&format!("{SCRATCH_BLOCKS} blocks of 4096 ")))
            .ok_or_else(|| {
                Error::new(format!(
                    "{arch}: the boot passed, and the nvme driver never said the scratch \
                     drive of {SCRATCH_BLOCKS} blocks of 4096 bytes came up"
                ))
            })?;
        println!("  {arch}: {}", up.trim());
    }
    Ok(())
}

/// An image whose init is zinc as an interactive shell, with the utilities
/// the gate types and every driver, and with `bootctl` and the installer
/// when `installer` says; the image and its kernel.
fn shell_image(arch: Arch, args: &Args, installer: bool) -> Result<(PathBuf, cargo::Kernel)> {
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose init is an interactive shell");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, "")?;
    let natives = native::build(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    if utilities.is_empty() {
        return Err(Error::new(
            "the image carries no utilities: `cargo xtask uutils` builds the ones this \
             gate types",
        ));
    }
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let extra = if installer {
        crate::installer::files(arch)?.ok_or_else(|| {
            Error::new("test-nvme --boot parts needs bootctl, which is not built for this arch")
        })?
    } else {
        Vec::new()
    };
    let archive =
        initramfs::build_with_utilities(Some(&shell), &natives, Some(&bytes), &utilities, &extra)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    Ok((image, kernel))
}

/// The scratch drive's bytes for `count` blocks from `block`: every 512
/// bytes the pattern of its own number.
fn scratch_bytes(block: u64, count: u64) -> Vec<u8> {
    let first = block * BLOCK / 512;
    let sectors = count * BLOCK / 512;
    let mut bytes = Vec::with_capacity((count * BLOCK) as usize);
    for number in first..first + sectors {
        bytes.extend_from_slice(&test_disk::sector(number));
    }
    bytes
}

/// What the virtio pattern disk holds in `count` 4096-byte blocks from
/// `block`: the same pattern, which is what makes it a source whose bytes
/// are known.
fn pattern_bytes(block: u64, count: u64) -> Vec<u8> {
    scratch_bytes(block, count)
}

/// Write blkdebug's rules: every read of [`BAD_BLOCK`] fails with `EIO`.
fn fault_rules(directory: &Path) -> Result<PathBuf> {
    let path = directory.join("nvme-fault.conf");
    let sector = BAD_BLOCK * BLOCK / 512;
    let rules = format!(
        "[inject-error]\nevent = \"read_aio\"\nerrno = \"5\"\nsector = \"{sector}\"\nonce = \"off\"\n"
    );
    std::fs::write(&path, rules)
        .map_err(|error| Error::new(format!("writing {}: {error}", path.display())))?;
    Ok(path)
}

/// Type `line` and read until [`END`]; the lines that followed.
fn ask(watching: &mut qemu::Watching<'_>, line: &str) -> std::result::Result<Vec<String>, String> {
    ask_until(watching, line, END)
}

/// Type `line` and read until a line holds `until`; the lines that followed.
fn ask_until(
    watching: &mut qemu::Watching<'_>,
    line: &str,
    until: &str,
) -> std::result::Result<Vec<String>, String> {
    let before = watching.after().len();
    watching
        .type_in(line.as_bytes())
        .map_err(|error| error.to_string())?;
    let ended = watching
        .read_more(Instant::now() + PATIENCE, |lines| {
            let lines = lines.get(before..).unwrap_or_default();
            lines.iter().any(|line| line.contains(until)) || restart::panicked(lines)
        })
        .map_err(|error| error.to_string())?;
    let lines = watching.after().get(before..).unwrap_or_default().to_vec();
    if restart::panicked(&lines) {
        return Err(format!("the kernel panicked after `{}`", line.trim()));
    }
    if !ended {
        return Err(format!("`{}` was never answered", line.trim()));
    }
    Ok(lines)
}

/// Wait for the shell, and find the two NVMe disks: the scratch drive by
/// its size, and `/`'s by the kernel's line.
fn ready(watching: &mut qemu::Watching<'_>) -> std::result::Result<String, String> {
    let deadline = Instant::now() + PATIENCE;
    if !watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        return Err("the shell never answered at the console".to_owned());
    }
    let up: Vec<&String> = watching
        .lines()
        .iter()
        .filter(|line| line.contains("nvme     ") && line.contains(UP))
        .collect();
    if !up.iter().any(|line| line.contains("blocks of 512 "))
        || !up.iter().any(|line| line.contains("blocks of 4096 "))
    {
        return Err(format!(
            "the nvme driver did not say both drives came up, one of 512-byte blocks and one \
             of 4096: it said {up:?}"
        ));
    }
    root_on_nvme(watching)?;
    disk_of(&list_nvme(watching)?, SCRATCH_SECTORS, "the scratch drive")
}

/// Require the kernel's line saying `/` is on an NVMe namespace, named as
/// Linux names it.
fn root_on_nvme(watching: &qemu::Watching<'_>) -> std::result::Result<(), String> {
    if watching
        .lines()
        .iter()
        .any(|line| line.contains("/ is btrfs on nvme") && line.contains("n1"))
    {
        Ok(())
    } else {
        Err("the kernel never said `/ is btrfs on nvme<C>n1`".to_owned())
    }
}

/// Every NVMe disk and partition in `/sys/class/block`, as `name:sectors`.
fn list_nvme(watching: &mut qemu::Watching<'_>) -> std::result::Result<Vec<String>, String> {
    let listed = ask(
        watching,
        "for d in /sys/class/block/nvme*; do read s < $d/size; \
         echo nvme-gate-'disk='${d##*/}:$s; done; echo nvme-gate-'end'\n",
    )?;
    Ok(listed
        .iter()
        .filter_map(|line| line.split_once(DISK_TAG))
        .map(|(_, rest)| rest.trim().to_owned())
        .collect())
}

/// The whole NVMe disk of `sectors` 512-byte sectors in `listed`.
fn disk_of(listed: &[String], sectors: u64, what: &str) -> std::result::Result<String, String> {
    let want = format!(":{sectors}");
    listed
        .iter()
        .filter_map(|entry| entry.strip_suffix(&want))
        .find(|name| name.ends_with("n1"))
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "no NVMe disk of {sectors} sectors in /sys/class/block ({listed:?}): {what} \
                 was not published as nvme<C>n1"
            )
        })
}

/// The partitioned drive's size in 512-byte sectors: 64 MiB, a size no
/// other disk of the machine has.
const GPT_SECTORS: u64 = 131_072;
/// Sectors in a MiB.
const MIB_SECTORS: u64 = 2048;
/// The partitioned drive's disk GUID.
const GPT_DISK_GUID: ferrix_partition::Guid =
    ferrix_partition::Guid::from_fields(0x6e76_6d65, 0x6770, 0x7400, 0x8000, 0x6665_7272_6978);

/// The partitioned drive's layout, as the customer's SN720 was given it on
/// 2026-10-11 and scaled down: each partition's type, name, first and last
/// sector (inclusive), in the order of the table, so partition N is entry
/// N - 1.
fn gpt_layout() -> [(ferrix_partition::Guid, &'static str, u64, u64); 6] {
    use ferrix_partition::{ESP, Guid, LINUX_FILESYSTEM};
    let swap = Guid::from_fields(0x0657_fd6d, 0xa4ab, 0x43c4, 0x84e5, 0x0933_c84b_4f4f);
    let control = Guid(ferrix_slots::gpt::CONTROL_TYPE);
    let slot = Guid(ferrix_slots::gpt::SLOT_TYPE);
    let mut at = MIB_SECTORS;
    let mut next = |mib: u64| {
        let range = (at, at + mib * MIB_SECTORS - 1);
        at += mib * MIB_SECTORS;
        range
    };
    let (p1, p2, p3, p4, p5) = (next(4), next(8), next(1), next(8), next(8));
    // The root takes the rest, up to the backup table's 33 sectors.
    let p6 = (at, GPT_SECTORS - 34);
    [
        (swap, "nazuna-swap", p1.0, p1.1),
        (ESP, "ferrix-esp", p2.0, p2.1),
        (control, "ferrix-bootctl", p3.0, p3.1),
        (slot, "ferrix-slot-a", p4.0, p4.1),
        (slot, "ferrix-slot-b", p5.0, p5.1),
        (LINUX_FILESYSTEM, "ferrix-root", p6.0, p6.1),
    ]
}

/// The control record the partitioned drive's first copy holds: slot A
/// good, no trial.
fn gpt_record() -> [u8; ferrix_slots::record::RECORD_LEN] {
    ferrix_slots::record::Record::initial(ferrix_slots::Slot::A).encode()
}

/// The partitioned drive's bytes: every sector its pattern
/// (`test_disk::sector` of its number), then the table, both copies, and
/// the record's first copy at the start of the control partition.
fn gpt_bytes() -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity((GPT_SECTORS * 512) as usize);
    for number in 0..GPT_SECTORS {
        bytes.extend_from_slice(&test_disk::sector(number));
    }
    let partitions: Vec<ferrix_partition::Partition> = gpt_layout()
        .iter()
        .enumerate()
        .map(|(index, &(kind, name, first, last))| {
            let seed = u8::try_from(index + 1).unwrap_or(0);
            ferrix_partition::Partition::new(
                kind,
                ferrix_partition::Guid::random([seed; 16]),
                first,
                last,
                name,
            )
        })
        .collect();
    let table = ferrix_partition::write_table(GPT_SECTORS, 512, GPT_DISK_GUID, &partitions)
        .map_err(|error| Error::new(format!("test-nvme's partition table: {error}")))?;
    let mut put = |at: u64, data: &[u8]| -> Result<()> {
        let start = usize::try_from(at).unwrap_or(usize::MAX);
        bytes
            .get_mut(start..start.saturating_add(data.len()))
            .ok_or_else(|| Error::new("test-nvme's partition table does not fit its disk"))?
            .copy_from_slice(data);
        Ok(())
    };
    for (lba, block) in table {
        put(lba * 512, &block)?;
    }
    let control = gpt_layout()[2].2;
    put(control * 512, &gpt_record())?;
    Ok(bytes)
}

/// `--boot parts`.
fn parts(watching: &mut qemu::Watching<'_>) -> std::result::Result<Vec<String>, String> {
    let deadline = Instant::now() + PATIENCE;
    if !watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        return Err("the shell never answered at the console".to_owned());
    }
    root_on_nvme(watching)?;
    let listed = list_nvme(watching)?;
    let disk = disk_of(&listed, GPT_SECTORS, "the partitioned drive")?;
    let layout = gpt_layout();
    // Every partition, by its Linux name and its size.
    for (index, &(_, label, first, last)) in layout.iter().enumerate() {
        let node = format!("{disk}p{}", index + 1);
        let entry = format!("{node}:{}", last - first + 1);
        if !listed.contains(&entry) {
            return Err(format!(
                "/sys/class/block has no {entry} ({label}): it lists {listed:?}"
            ));
        }
    }
    // The first and last sector of each, read through its own node.
    for (index, &(_, label, first, last)) in layout.iter().enumerate() {
        let node = format!("{disk}p{}", index + 1);
        let first_bytes = if index == 2 {
            gpt_record().to_vec()
        } else {
            test_disk::sector(first).to_vec()
        };
        require_read(
            watching,
            &node,
            (512, 0, 1),
            &first_bytes,
            &format!("the first sector of {label}"),
        )?;
        require_read(
            watching,
            &node,
            (512, last - first, 1),
            &test_disk::sector(last),
            &format!("the last sector of {label}"),
        )?;
    }
    // bootctl finds the layout by its types and names its nodes.
    let said = ask(
        watching,
        "/sbin/bootctl status; echo nvme-gate-'status='$?; echo nvme-gate-'end'\n",
    )?;
    if status(&said) != Some(0) {
        return Err(format!("bootctl status did not exit 0: {said:?}"));
    }
    let named = format!("(control /dev/{disk}p3, slot a /dev/{disk}p4, slot b /dev/{disk}p5)");
    for wanted in [
        format!("disk:    /dev/{disk} "),
        named.clone(),
        "record:  sequence ".to_owned(),
        "good:    a".to_owned(),
        format!("slot a:  /dev/{disk}p4 (8 MiB)"),
        format!("slot b:  /dev/{disk}p5 (8 MiB)"),
    ] {
        if !said.iter().any(|line| line.contains(&wanted)) {
            return Err(format!(
                "bootctl status did not say `{wanted}`: it said {said:?}"
            ));
        }
    }
    Ok(vec![
        format!(
            "/dev/{disk}: partitions {disk}p1 to {disk}p6 published with their sizes; the \
             first and last sector of each read back as written, the control partition's \
             first the record"
        ),
        format!("bootctl status found the layout on /dev/{disk}: {named}"),
    ])
}

/// What `--boot parts` requires of the serial log: the kernel's line for
/// each partition it published, with its size.
fn after_parts(lines: &[String]) -> std::result::Result<(), String> {
    for (index, &(_, label, first, last)) in gpt_layout().iter().enumerate() {
        let wanted = format!(
            "p{} is a partition of {} sectors",
            index + 1,
            last - first + 1
        );
        if !lines
            .iter()
            .any(|line| line.contains("disks    nvme") && line.contains(&wanted))
        {
            return Err(format!(
                "the kernel never said `disks    nvme<C>n1{wanted}` for {label}"
            ));
        }
    }
    Ok(())
}

/// The SHA-256 the guest prints for `count` blocks of `bytes` bytes from
/// block `skip` of `/dev/<disk>`, read in one `dd`.
fn guest_sum(
    watching: &mut qemu::Watching<'_>,
    disk: &str,
    bytes: u64,
    skip: u64,
    count: u64,
) -> std::result::Result<String, String> {
    let lines = ask(
        watching,
        &format!(
            "dd if=/dev/{disk} bs={bytes} skip={skip} count={count} 2>/dev/null | sha256sum; \
             echo nvme-gate-'end'\n"
        ),
    )?;
    sums(&lines).into_iter().next().ok_or_else(|| {
        format!("reading {count} x {bytes} bytes at {skip} of /dev/{disk} printed no checksum")
    })
}

/// Every SHA-256 `sha256sum` printed for its standard input in `lines`.
fn sums(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let (sum, _) = line.trim().split_once("  -")?;
            // Lower-case digits only: the prompt's escape codes, which may
            // come right before the answer, end in upper-case letters.
            let digit = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
            let sum = sum.rsplit(|c: char| !digit(c)).next()?;
            (sum.len() == 64).then(|| sum.to_owned())
        })
        .collect()
}

/// Require the guest's read of `count` blocks of `bytes` bytes at `skip`
/// to be `expected`'s bytes.
fn require_read(
    watching: &mut qemu::Watching<'_>,
    disk: &str,
    (bytes, skip, count): (u64, u64, u64),
    expected: &[u8],
    what: &str,
) -> std::result::Result<(), String> {
    let got = guest_sum(watching, disk, bytes, skip, count)?;
    let want = sha256::hex(expected);
    if got == want {
        Ok(())
    } else {
        Err(format!(
            "{what}: /dev/{disk} read back with SHA-256 {got}, and the bytes there have {want}"
        ))
    }
}

/// The exit status a line of `lines` reports.
fn status(lines: &[String]) -> Option<i32> {
    lines.iter().find_map(|line| {
        let (_, rest) = line.split_once(STATUS_TAG)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// Where the gate writes on the scratch drive, in blocks, and how many.
const WRITE_AT: u64 = 2000;
const WRITE_BLOCKS: u64 = 24;
/// The block of the virtio pattern disk the written bytes come from.
const WRITE_FROM: u64 = 3000;
/// The 128 KiB read: 32 blocks from block 512, as one request.
const LARGE: (u64, u64, u64) = (32 * BLOCK, 16, 1);
/// What the load loop reads each time round: 2 MiB from block 64.
const LOAD: (u64, u64, u64) = (32 * BLOCK, 2, 16);

/// `--boot io`.
fn io(watching: &mut qemu::Watching<'_>) -> std::result::Result<Vec<String>, String> {
    let disk = ready(watching)?;
    // Reads of known blocks.
    let last = SCRATCH_BLOCKS - 1;
    require_read(
        watching,
        &disk,
        (BLOCK, 0, 1),
        &scratch_bytes(0, 1),
        "the first block",
    )?;
    require_read(
        watching,
        &disk,
        (BLOCK, last, 1),
        &scratch_bytes(last, 1),
        "the last block",
    )?;
    require_read(
        watching,
        &disk,
        LARGE,
        &scratch_bytes(LARGE.1 * 32, 32),
        "the 128 KiB read, which takes a PRP list",
    )?;
    // A write, flushed, and read back.
    let wrote = ask(
        watching,
        &format!(
            "dd if=/dev/vda of=/dev/{disk} bs={BLOCK} skip={WRITE_FROM} seek={WRITE_AT} \
             count={WRITE_BLOCKS} conv=notrunc,fsync 2>/dev/null; \
             echo nvme-gate-'status='$?; echo nvme-gate-'end'\n"
        ),
    )?;
    if status(&wrote) != Some(0) {
        return Err(format!(
            "writing {WRITE_BLOCKS} blocks to /dev/{disk} with a flush did not exit 0: {wrote:?}"
        ));
    }
    require_read(
        watching,
        &disk,
        (BLOCK, WRITE_AT, WRITE_BLOCKS),
        &pattern_bytes(WRITE_FROM, WRITE_BLOCKS),
        "the blocks just written",
    )?;
    let (reads, before_last) = kills_under_load(watching, &disk)?;
    // Still right after the kills.
    require_read(
        watching,
        &disk,
        (BLOCK, WRITE_AT, WRITE_BLOCKS),
        &pattern_bytes(WRITE_FROM, WRITE_BLOCKS),
        "the written blocks, after two restarts",
    )?;
    Ok(vec![
        format!(
            "/dev/{disk} (4096-byte blocks): the first, the last and 128 KiB in one request \
             read back as written; {WRITE_BLOCKS} blocks written with a flush read back"
        ),
        format!(
            "both nvme drivers killed twice under a reading loop: {} reads of 2 MiB, all \
             right, {} of them before the second kill; / on the NVMe root took a file after \
             each",
            reads, before_last
        ),
    ])
}

/// Kill both drivers twice while a loop reads the scratch drive, and
/// require every read of the loop right and some made after the last kill;
/// the reads made, and how many of them before the last kill.
fn kills_under_load(
    watching: &mut qemu::Watching<'_>,
    disk: &str,
) -> std::result::Result<(usize, usize), String> {
    let expected = sha256::hex(&scratch_bytes(LOAD.1 * 32, LOAD.2 * 32));
    let started = watching.after().len();
    watching
        .type_in(
            format!(
                "(while [ ! -e {STOP_FILE} ]; do dd if=/dev/{disk} bs={} skip={} count={} \
                 2>/dev/null | sha256sum; done; echo nvme-gate-'load-over') &\n",
                LOAD.0, LOAD.1, LOAD.2
            )
            .as_bytes(),
        )
        .map_err(|error| error.to_string())?;
    let mut killed = Vec::new();
    let mut rounds = Vec::new();
    for round in 1..=2 {
        let before = sums(watching.after().get(started..).unwrap_or_default()).len();
        restart::kill_and_expect_back(watching, Kind::Nvme, round, &mut killed)?;
        restart::write_and_read_back(watching, round)?;
        rounds.push(before);
    }
    let over = ask_until(watching, &format!(": > {STOP_FILE}; sync\n"), LOAD_OVER)?;
    drop(over);
    let read = sums(watching.after().get(started..).unwrap_or_default());
    let wrong = read.iter().filter(|sum| **sum != expected).count();
    if read.is_empty() || wrong > 0 {
        return Err(format!(
            "the loop reading /dev/{disk} across the kills made {} reads of 2 MiB, {wrong} of \
             them wrong: a request the kernel kept for the next driver came back with other \
             bytes, or none was made",
            read.len()
        ));
    }
    if read.len() <= rounds.last().copied().unwrap_or(0) {
        return Err(format!(
            "the loop reading /dev/{disk} made {} reads before the second kill and none \
             after it: the disk did not serve again",
            read.len()
        ));
    }
    Ok((read.len(), rounds.last().copied().unwrap_or(0)))
}

/// What `--boot io` requires of the host's side once the guest is done:
/// the written blocks in the image with their neighbours as they were,
/// Flush commands in QEMU's trace, and no request failed.
fn after_io(lines: &[String], scratch: &Path, trace: &Path) -> std::result::Result<(), String> {
    let image = std::fs::read(scratch).map_err(|error| error.to_string())?;
    let at = |block: u64, count: u64| {
        let start = (block * BLOCK) as usize;
        image
            .get(start..start + (count * BLOCK) as usize)
            .unwrap_or_default()
    };
    if at(WRITE_AT, WRITE_BLOCKS) != pattern_bytes(WRITE_FROM, WRITE_BLOCKS) {
        return Err(format!(
            "the scratch image does not hold the {WRITE_BLOCKS} blocks the guest wrote at \
             block {WRITE_AT} and flushed"
        ));
    }
    if at(WRITE_AT - 1, 1) != scratch_bytes(WRITE_AT - 1, 1)
        || at(WRITE_AT + WRITE_BLOCKS, 1) != scratch_bytes(WRITE_AT + WRITE_BLOCKS, 1)
    {
        return Err(format!(
            "a block beside the {WRITE_BLOCKS} written at {WRITE_AT} changed in the scratch image"
        ));
    }
    let traced = std::fs::read_to_string(trace).unwrap_or_default();
    let flushes = traced
        .lines()
        .filter(|line| line.contains("pci_nvme_flush_ns"))
        .count();
    if flushes == 0 {
        return Err(format!(
            "QEMU's trace ({}) shows no Flush command reaching an NVMe controller: the \
             guest's fsync and sync were answered without one",
            trace.display()
        ));
    }
    println!("  qemu: {flushes} Flush commands reached the NVMe controllers, by QEMU's trace");
    if let Some(failed) = lines.iter().find(|line| line.contains(FAILED)) {
        return Err(format!("the driver failed a request: {}", failed.trim()));
    }
    // Said and not judged: on a host too loaded to run every virtual
    // processor, the one an interrupt is routed to can stand still for
    // longer than the driver's watch waits on another, and the watch then
    // takes the completions first (seen at a host load of 50). On a quiet
    // host it would mean a lost interrupt.
    let late = lines
        .iter()
        .filter(|line| line.contains(LATE_INTERRUPT))
        .count();
    if late > 0 {
        println!(
            "  note: {late} times the driver's watch took completions before their interrupt \
             came; expected only on a host too loaded to run every virtual processor"
        );
    }
    Ok(())
}

/// `--boot fault`.
fn fault(watching: &mut qemu::Watching<'_>) -> std::result::Result<Vec<String>, String> {
    let disk = ready(watching)?;
    require_read(
        watching,
        &disk,
        (BLOCK, BAD_BLOCK - 1, 1),
        &scratch_bytes(BAD_BLOCK - 1, 1),
        "the block before the bad one",
    )?;
    let bad = ask(
        watching,
        &format!(
            "dd if=/dev/{disk} of=/dev/null bs={BLOCK} skip={BAD_BLOCK} count=1 2>/dev/null; \
             echo nvme-gate-'status='$?; echo nvme-gate-'end'\n"
        ),
    )?;
    match status(&bad) {
        Some(0) => {
            return Err(format!(
                "reading block {BAD_BLOCK} of /dev/{disk}, which QEMU fails, exited 0: the \
                 controller's error did not reach the reader"
            ));
        }
        Some(_) => {}
        None => return Err("the read of the bad block never said its status".to_owned()),
    }
    require_read(
        watching,
        &disk,
        (BLOCK, BAD_BLOCK + 1, 1),
        &scratch_bytes(BAD_BLOCK + 1, 1),
        "the block after the bad one, once it had failed",
    )?;
    Ok(vec![format!(
        "/dev/{disk}: the read QEMU fails failed in the guest, and the blocks beside it read \
         back right before and after"
    )])
}

/// What `--boot fault` requires of the serial log: the driver said the
/// request failed, and it did not reset or give up the controller for it.
fn after_fault(lines: &[String]) -> std::result::Result<(), String> {
    if !lines.iter().any(|line| line.contains(FAILED)) {
        return Err(format!(
            "the driver never said `{FAILED}... failed` for the read QEMU failed"
        ));
    }
    if let Some(line) = lines
        .iter()
        .find(|line| line.contains("nvme     controller reset") || line.contains("given up"))
    {
        return Err(format!(
            "one failed read made the driver reset or give up the controller: {}",
            line.trim()
        ));
    }
    Ok(())
}

/// 4 KiB reads in one timed `dd`.
const BENCH_READS: u64 = 4096;
/// Timed runs of each disk, after one that is not counted.
const BENCH_ROUNDS: usize = 5;

/// `--boot bench`.
fn bench(watching: &mut qemu::Watching<'_>) -> std::result::Result<Vec<String>, String> {
    let disk = ready(watching)?;
    let mut virtio = Vec::new();
    let mut nvme = Vec::new();
    for round in 0..=BENCH_ROUNDS {
        for (name, results) in [("vda", &mut virtio), (disk.as_str(), &mut nvme)] {
            let nanos = timed_reads(watching, name)?;
            // The first round warms both paths and is not counted.
            if round > 0 {
                results.push(nanos);
            }
        }
    }
    let load = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let seam = watching
        .lines()
        .iter()
        .find(|line| line.contains("  seam     a 4 KiB read"))
        .map(|line| line.trim().to_owned());
    let mut said = vec![
        format!(
            "a 4 KiB read, the mean of {BENCH_READS} in one dd, {BENCH_ROUNDS} runs each in \
             turn, host load {}",
            load.split_whitespace()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        ),
        summary("virtio-blk (vda, 512-byte blocks)", &mut virtio),
        summary(&format!("nvme ({disk}, 4096-byte blocks)"), &mut nvme),
    ];
    if let Some(seam) = seam {
        said.push(format!(
            "the kernel's own measurement of vda at boot: {seam}"
        ));
    }
    Ok(said)
}

/// The mean time of one 4 KiB read of `/dev/<disk>`, in nanoseconds, as
/// `dd` timed [`BENCH_READS`] of them.
fn timed_reads(watching: &mut qemu::Watching<'_>, disk: &str) -> std::result::Result<u64, String> {
    let lines = ask(
        watching,
        &format!(
            "dd if=/dev/{disk} of=/dev/null bs={BLOCK} count={BENCH_READS}; \
             echo nvme-gate-'end'\n"
        ),
    )?;
    lines
        .iter()
        .find_map(|line| {
            let (_, rest) = line.split_once(" copied, ")?;
            let seconds: f64 = rest.split_whitespace().next()?.parse().ok()?;
            Some((seconds * 1e9 / BENCH_READS as f64) as u64)
        })
        .ok_or_else(|| format!("dd of /dev/{disk} did not say how long it took: {lines:?}"))
}

/// One disk's runs, as a line: each run's mean, their median and the least.
fn summary(what: &str, nanos: &mut [u64]) -> String {
    nanos.sort_unstable();
    let micros = |nanos: u64| format!("{}.{}", nanos / 1000, (nanos % 1000) / 100);
    let median = nanos.get(nanos.len() / 2).copied().unwrap_or(0);
    let least = nanos.first().copied().unwrap_or(0);
    let all: Vec<String> = nanos.iter().map(|&nanos| micros(nanos)).collect();
    format!(
        "{what}: median {} us, least {} us (runs: {} us)",
        micros(median),
        micros(least),
        all.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_partitioned_drive_reads_back_as_its_layout_and_record() {
        let bytes = gpt_bytes().expect("written");
        assert_eq!(bytes.len() as u64, GPT_SECTORS * 512);
        let header = ferrix_partition::parse_header(&bytes[512..1024], GPT_SECTORS)
            .expect("a primary header");
        let at = usize::try_from(header.entries_at * 512).expect("fits");
        let entries = ferrix_partition::parse_entries(&header, &bytes[at..]).expect("entries");
        let layout = gpt_layout();
        assert_eq!(entries.len(), layout.len());
        for ((_, entry), (kind, _, first, last)) in entries.iter().zip(layout) {
            assert_eq!(
                (entry.type_guid, entry.first, entry.last),
                (kind, first, last)
            );
        }
        let control = usize::try_from(layout[2].2 * 512).expect("fits");
        assert_eq!(&bytes[control..control + 512], &gpt_record()[..]);
        // Every partition's last sector, and the first of all but the
        // control partition, is its pattern: what the guest compares.
        for (index, &(_, _, first, last)) in layout.iter().enumerate() {
            let sector = |n: u64| &bytes[(n * 512) as usize..(n * 512 + 512) as usize];
            assert_eq!(sector(last), test_disk::sector(last).as_slice());
            if index != 2 {
                assert_eq!(sector(first), test_disk::sector(first).as_slice());
            }
        }
    }

    #[test]
    fn a_checksum_is_read_off_sha256sums_line_and_not_off_the_typed_one() {
        let sum = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let said = lines(&[
            "dd if=/dev/vdd bs=4096 skip=0 count=1 2>/dev/null | sha256sum; echo nvme-gate-'end'",
            &format!("{sum}  -"),
            // The prompt's escape codes may come before the answer.
            &format!("\u{1b}[00m \u{1b}[J\u{1b}[5C{sum}  -"),
            "0123  -",
            "nvme-gate-end",
        ]);
        assert_eq!(sums(&said), [sum, sum]);
    }

    #[test]
    fn a_status_is_the_number_after_its_tag() {
        let said = lines(&["echo nvme-gate-'status='$?", "nvme-gate-status=1", END]);
        assert_eq!(status(&said), Some(1));
        assert_eq!(status(&lines(&["nothing"])), None);
    }

    #[test]
    fn the_scratch_drive_is_the_pattern_of_its_512_byte_sectors() {
        let block = scratch_bytes(3, 1);
        assert_eq!(block.len(), 4096);
        assert_eq!(block[..512], test_disk::sector(24));
        assert_eq!(block[3584..], test_disk::sector(31));
    }

    #[test]
    fn a_run_is_summarised_by_its_median_and_its_least() {
        let mut runs = [90_500, 70_100, 80_000];
        assert_eq!(
            summary("disk", &mut runs),
            "disk: median 80.0 us, least 70.1 us (runs: 70.1, 80.0, 90.5 us)"
        );
    }
}
