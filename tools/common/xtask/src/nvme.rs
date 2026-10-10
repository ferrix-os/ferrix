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
    Up,
}

impl Mode {
    fn asked(boot: Option<&str>) -> Result<Mode> {
        match boot {
            None | Some("io") => Ok(Mode::Io),
            Some("fault") => Ok(Mode::Fault),
            Some("bench") => Ok(Mode::Bench),
            Some("up") => Ok(Mode::Up),
            Some(other) => Err(Error::new(format!(
                "test-nvme --boot takes io, fault, bench or up, not `{other}`"
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
    let (image, kernel) = shell_image(arch, args)?;

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
    let checker = match mode {
        Mode::Io => Some(btrfs_check::Checker::required()?),
        Mode::Fault | Mode::Bench | Mode::Up => None,
    };
    println!(
        "  {arch}: test-nvme --boot {} (timeout {}s)",
        match mode {
            Mode::Io => "io",
            Mode::Fault => "fault",
            Mode::Bench => "bench",
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
/// the gate types and every driver; the image and its kernel.
fn shell_image(arch: Arch, args: &Args) -> Result<(PathBuf, cargo::Kernel)> {
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
    let archive =
        initramfs::build_with_utilities(Some(&shell), &natives, Some(&bytes), &utilities, &[])?;
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
    if !watching
        .lines()
        .iter()
        .any(|line| line.contains("/ is btrfs on vd"))
    {
        return Err("the kernel never said `/ is btrfs on` a disk".to_owned());
    }
    let listed = ask(
        watching,
        "for d in /sys/class/block/vd*; do read s < $d/size; \
         echo nvme-gate-'disk='${d##*/}:$s; done; echo nvme-gate-'end'\n",
    )?;
    let want = format!(":{SCRATCH_SECTORS}");
    listed
        .iter()
        .filter_map(|line| line.split_once(DISK_TAG))
        .map(|(_, rest)| rest.trim())
        .find_map(|entry| entry.strip_suffix(&want))
        .map(str::to_owned)
        .ok_or_else(|| {
            format!(
                "no disk of {SCRATCH_SECTORS} sectors in /sys/class/block: the scratch drive \
                 was not published"
            )
        })
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
