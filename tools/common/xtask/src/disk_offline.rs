//! `test-disk-offline`: a disk that stops answering is taken offline, and the
//! machine answers at once rather than waiting on it (`docs/BLOCK-RING.md`,
//! "A disk that stops answering"; BACKLOG p1-034).
//!
//! On the N150 box on 2026-10-25 a failed write left `/` read-only and the
//! box answered nothing over ssh for about ten minutes: every request to a
//! disk that does not answer waited the block ring's 30 s patience, and
//! every file read the page cache missed waited behind it. Linux's SCSI
//! error handler takes such a device offline once its retries and resets
//! have failed, and from then on fails its I/O at once.
//!
//! Two boots of one image, a shell as init, `/` on a btrfs root:
//!
//! 1. The root is made, and two probe files are written and synced; nothing
//!    of them is read again, so the next boot finds them on the disk only.
//! 2. A fresh boot reads `/off-cached` (and runs `sync`, so its program is
//!    cached too), then stalls the root disk with QMP's
//!    `block_set_io_throttle` at one byte a second -- a drive that hangs
//!    behind a driver that stays alive -- and starts a `sync` in the
//!    background, which waits on the disk. Then it requires:
//!    * the shell answers a builtin (`echo $((6 * 7))`) within
//!      [`QUICK`] while the disk hangs;
//!    * the kernel says the disk is offline within [`OFFLINE_BY`];
//!    * then, each within [`QUICK`]: the cached file reads back (the shell's
//!      `read` builtin), an uncached probe answers an error, a write
//!      answers an error, and the shell answers;
//!    * `/sys/block/vdd/state` reads `running` before the stall and
//!      `offline` once the kernel said so;
//!    * the throttle is lifted, the stalled commands complete late, and the
//!      state still reads `offline` and an uncached probe still answers an
//!      error: only a new driver brings an offline disk back, not a late
//!      completion.
//!
//!    Every command timed is a zinc builtin, or a program run before the
//!    stall (`sync`).
//! 3. On the host: the volume checks clean (`btrfs check`), and the committed
//!    tree has no `/off-sentinel`, written after the stall and answered as
//!    failed (`btrfs inspect-internal dump-tree`).
//!
//! x86-64 only, as `test-rename-crash` is: `sync` is uutils'.

use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, btrfs_check, btrfs_disk, cargo, display, fat, initramfs, native, qemu};
use crate::{uutils, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-disk-offline",
    help_at: 186,
    help: &[
        "test-disk-offline  Stall the btrfs root's disk through QMP and require it taken offline, the shell",
        "              answering within seconds, cached files read and the rest failing fast (x86_64)",
    ],
    run: |_, args| test(args),
}];

/// How long the boot's ordinary lines may take to be answered.
const PATIENCE: Duration = Duration::from_secs(60);

/// How long a line may take while, or after, the disk hangs.
const QUICK: Duration = Duration::from_secs(5);

/// How long after the stall the disk must be offline: the block ring's 30 s
/// patience, and the sync's first write reaching the disk.
const OFFLINE_BY: Duration = Duration::from_secs(75);

/// What the kernel prints for a disk taken offline.
const OFFLINE: &str = "offline: its driver answered nothing";

/// What the kernel prints once `/` is the btrfs root.
const ROOT_LINE: &str = "/ is btrfs on";

/// What the guest's answers begin with; typed with a quote in the middle so
/// the echo of the typed line never matches.
const TAG: &str = "off-gate-";

/// The root drive's QEMU id (`qemu::attach_root_disk`).
const ROOT_DRIVE: &str = "btrfsroot";

/// `test-disk-offline`.
fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-disk-offline runs on x86-64 only: `sync` comes from uutils, built for x86-64 \
             alone (docs/UUTILS.md D3)",
        ));
    }
    let checker = btrfs_check::Checker::required()?;
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, "")?;
    let natives = native::build(arch, args.release)?;
    let utilities = uutils::carried(arch)?;
    if utilities.is_empty() {
        return Err(Error::new(
            "the image carries no utilities: `cargo xtask uutils` builds the ones this gate types",
        ));
    }
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let archive =
        initramfs::build_with_utilities(Some(&shell), &natives, Some(&bytes), &utilities, &[])?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;
    let mut booted = args.clone();
    booted.btrfs_root = true;
    booted.qmp_port = Some(display::free_port()?);

    btrfs_disk::keep_test_root(false);
    let outcome = boots(arch, &image, &kernel, &booted);
    btrfs_disk::keep_test_root(false);
    outcome?;
    let root = btrfs_disk::test_root_path(arch);
    checker.run(&root, arch)?;
    sentinel_not_committed(&root)?;
    println!(
        "  {arch}: a disk that stopped answering was taken offline; the shell answered, cached \
         files read and the rest failed within {} s; the failed write is not on the volume",
        QUICK.as_secs()
    );
    Ok(())
}

/// The two boots.
fn boots(arch: Arch, image: &std::path::Path, kernel: &std::path::Path, args: &Args) -> Result<()> {
    println!("  {arch}: boot 1 of 2: the root made, probe files written");
    boot(arch, image, kernel, args, |watching| {
        ready(watching)?;
        let answer = say(
            watching,
            "echo probe-one > /off-probe1 && echo probe-two > /off-probe2 && sync && \
             echo off-gate-'probes=yes'\n",
            "probes",
            PATIENCE,
        )?;
        if answer.0 != "yes" {
            return Err(format!("writing the probes answered {:?}", answer.0));
        }
        watching.stop_when_done();
        Ok(())
    })?;
    btrfs_disk::keep_test_root(true);

    println!("  {arch}: boot 2 of 2: the root disk stalled through QMP");
    let port = args
        .qmp_port
        .ok_or_else(|| Error::new("test-disk-offline needs a QMP port"))?;
    boot(arch, image, kernel, args, |watching| {
        stalled(watching, port)
    })
}

/// Boot 2's hook: the stall, and what must answer during and after it.
fn stalled(watching: &mut qemu::Watching<'_>, port: u16) -> std::result::Result<(), String> {
    ready(watching)?;
    let (cached, _) = say(
        watching,
        "echo cached-payload > /off-cached && sync && read l < /off-cached && \
         echo off-gate-'pre='$l\n",
        "pre",
        PATIENCE,
    )?;
    if cached != "cached-payload" {
        return Err(format!(
            "the cached file read back {cached:?} before the stall"
        ));
    }
    let (state, _) = say(
        watching,
        "read s < /sys/block/vdd/state; echo off-gate-'state='$s\n",
        "state",
        PATIENCE,
    )?;
    if state != "running" {
        return Err(format!(
            "/sys/block/vdd/state read {state:?} before the stall"
        ));
    }
    let mut qmp = display::Qmp::connect(port, Instant::now() + PATIENCE)
        .map_err(|error| error.to_string())?;
    throttle(&mut qmp, 1)?;
    println!("  the root disk is throttled to one byte a second");
    watching
        .type_in(b"echo sentinel-7f3a9 > /off-sentinel; sync &\n")
        .map_err(|error| error.to_string())?;
    let (alive, took) = say(
        watching,
        "echo off-gate-'alive='$((6 * 7))\n",
        "alive",
        QUICK,
    )?;
    if alive != "42" {
        return Err(format!("the shell answered {alive:?} while the disk hung"));
    }
    println!(
        "  the shell answered in {} ms while the disk hung",
        took.as_millis()
    );
    let offline = watching
        .read_more(Instant::now() + OFFLINE_BY, |lines| {
            lines.iter().any(|line| line.contains(OFFLINE))
        })
        .map_err(|error| error.to_string())?;
    if !offline {
        return Err(format!(
            "the disk was not taken offline within {} s of the stall: no `{OFFLINE}`",
            OFFLINE_BY.as_secs()
        ));
    }
    offline_answers(watching, &mut qmp)?;
    watching.stop_when_done();
    Ok(())
}

/// Once the kernel said the disk is offline: what must answer at once, and
/// that lifting the stall brings nothing back.
fn offline_answers(
    watching: &mut qemu::Watching<'_>,
    qmp: &mut display::Qmp,
) -> std::result::Result<(), String> {
    disk_state(
        watching,
        "offline",
        "offline",
        "once the kernel said offline",
    )?;
    let (back, _) = quick(
        watching,
        "read l < /off-cached; echo off-gate-'cached='$l\n",
        "cached",
    )?;
    if back != "cached-payload" {
        return Err(format!(
            "the cached file read back {back:?} once the disk was offline"
        ));
    }
    let (status, _) = quick(
        watching,
        "read l < /off-probe1; echo off-gate-'uncached='$?\n",
        "uncached",
    )?;
    if status == "0" {
        return Err("an uncached file read back from an offline disk".to_owned());
    }
    let (status, _) = quick(
        watching,
        "echo x > /off-new; echo off-gate-'write='$?\n",
        "write",
    )?;
    if status == "0" {
        return Err("a write to an offline disk's volume succeeded".to_owned());
    }
    throttle(qmp, 0)?;
    println!("  the throttle is lifted");
    let _ = watching
        .read_more(Instant::now() + Duration::from_secs(5), |_| false)
        .map_err(|error| error.to_string())?;
    disk_state(
        watching,
        "lifted",
        "offline",
        "once the throttle was lifted: the disk came back online without a new driver",
    )?;
    let (status, _) = quick(
        watching,
        "read l < /off-probe2; echo off-gate-'after='$?\n",
        "after",
    )?;
    if status == "0" {
        return Err(
            "an uncached file read back once the throttle was lifted: the disk came back \
             online without a new driver"
                .to_owned(),
        );
    }
    let (alive, _) = quick(watching, "echo off-gate-'end='$((6 * 7))\n", "end")?;
    if alive != "42" {
        return Err(format!("the shell answered {alive:?} at the end"));
    }
    Ok(())
}

/// Set the root drive's throttle to `bps` bytes a second, 0 for none.
fn throttle(qmp: &mut display::Qmp, bps: u64) -> std::result::Result<(), String> {
    let arguments = format!(
        "{{\"device\":\"{ROOT_DRIVE}\",\"bps\":{bps},\"bps_rd\":0,\"bps_wr\":0,\
         \"iops\":0,\"iops_rd\":0,\"iops_wr\":0}}"
    );
    let answer = qmp
        .execute("block_set_io_throttle", Some(&arguments))
        .map_err(|error| error.to_string())?;
    if answer.contains("\"error\"") {
        return Err(format!("block_set_io_throttle answered {answer}"));
    }
    Ok(())
}

/// The committed tree must hold no `/off-sentinel`: the names in the fs
/// tree the image's superblock reaches, as host `btrfs inspect-internal
/// dump-tree` prints them, which must hold the probes and not the sentinel.
fn sentinel_not_committed(image: &std::path::Path) -> Result<()> {
    let output = Command::new("btrfs")
        .args(["inspect-internal", "dump-tree", "-t", "fs"])
        .arg(image)
        .output()
        .map_err(|error| Error::new(format!("running btrfs inspect-internal: {error}")))?;
    let tree = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || !tree.contains("name: off-probe1") {
        return Err(Error::new(format!(
            "btrfs inspect-internal dump-tree could not list the volume ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if tree.contains("name: off-sentinel") {
        return Err(Error::new(
            "the committed volume holds /off-sentinel, a write answered as failed",
        ));
    }
    Ok(())
}

/// One boot: `hook` types at the shell, and its failure is the gate's.
fn boot(
    arch: Arch,
    image: &std::path::Path,
    kernel: &std::path::Path,
    args: &Args,
    mut hook: impl FnMut(&mut qemu::Watching<'_>) -> std::result::Result<(), String>,
) -> Result<()> {
    let mut failure = None;
    let lines = qemu::watch_then(
        arch,
        image,
        kernel,
        args,
        qemu::SUCCESS_MARKER,
        |watching| {
            if let Err(why) = hook(watching) {
                watching.stop_when_done();
                failure = Some(why);
            }
            Ok(())
        },
    )?;
    if let Some(panic) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Err(Error::new(format!(
            "{arch}: the kernel panicked: {}",
            panic.trim()
        )));
    }
    match failure {
        Some(why) => Err(Error::new(format!(
            "{arch}: test-disk-offline: {why}\n  The whole transcript is above and in the serial \
             log."
        ))),
        None => Ok(()),
    }
}

/// Wait until `/` is the btrfs root and the shell reads the console.
fn ready(watching: &mut qemu::Watching<'_>) -> std::result::Result<(), String> {
    let deadline = Instant::now() + PATIENCE;
    let rooted = |lines: &[String]| lines.iter().any(|line| line.contains(ROOT_LINE));
    let seen = rooted(watching.lines())
        || watching
            .read_more(deadline, rooted)
            .map_err(|error| error.to_string())?;
    if !seen {
        return Err(format!("`/` never became the btrfs root: no `{ROOT_LINE}`"));
    }
    if watching
        .wait_for_shell(deadline)
        .map_err(|error| error.to_string())?
    {
        Ok(())
    } else {
        Err("the shell never answered at the console".to_owned())
    }
}

/// Require `/sys/block/vdd/state` to read `want` within [`QUICK`], typed with
/// the answer tagged `key`; `when` says when, for the failure.
fn disk_state(
    watching: &mut qemu::Watching<'_>,
    key: &str,
    want: &str,
    when: &str,
) -> std::result::Result<(), String> {
    let line = format!("read s < /sys/block/vdd/state; echo off-gate-'{key}='$s\n");
    let (state, _) = quick(watching, &line, key)?;
    if state == want {
        Ok(())
    } else {
        Err(format!("/sys/block/vdd/state read {state:?} {when}"))
    }
}

/// [`say`] within [`QUICK`], the bound while the disk is offline.
fn quick(
    watching: &mut qemu::Watching<'_>,
    line: &str,
    key: &str,
) -> std::result::Result<(String, Duration), String> {
    let answer = say(watching, line, key, QUICK)?;
    println!("  {key}: {:?} in {} ms", answer.0, answer.1.as_millis());
    Ok(answer)
}

/// Type `line`, and give back the word after `off-gate-<key>=` in the first
/// answer that has it, and how long it took; a failure past `within`.
fn say(
    watching: &mut qemu::Watching<'_>,
    line: &str,
    key: &str,
    within: Duration,
) -> std::result::Result<(String, Duration), String> {
    let tag = format!("{TAG}{key}=");
    let before = watching.after().len();
    let started = Instant::now();
    watching
        .type_in(line.as_bytes())
        .map_err(|error| error.to_string())?;
    let answered = watching
        .read_more(started + within, |lines| {
            lines
                .get(before..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(&tag))
        })
        .map_err(|error| error.to_string())?;
    if !answered {
        return Err(format!(
            "no answer to {:?} within {} s",
            line.trim(),
            within.as_secs()
        ));
    }
    let took = started.elapsed();
    let word = watching
        .after()
        .get(before..)
        .unwrap_or_default()
        .iter()
        .find_map(|line| line.split_once(&tag))
        .map(|(_, rest)| {
            rest.split_whitespace()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_default();
    Ok((word, took))
}
