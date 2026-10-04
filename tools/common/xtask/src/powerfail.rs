//! Stage 12's exit, the power-fail half: kill QEMU at a random point inside
//! a transaction, remount, replay, and let `btrfs check` judge — over as many
//! seeds as `--seeds` asks for.
//!
//! One seed is two boots of the same image with the same writable disk:
//!
//! 1. `ferrix.btrfs=churn`: the guest mounts the blank volume and rewrites
//!    files and makes them durable, with `fsync` (a log commit) and `sync` (a
//!    whole commit) both, until it is stopped. This waits for it to say it has
//!    started, waits a random time the seed picks, and kills QEMU — no
//!    shutdown, no chance to finish anything.
//! 2. Host `btrfs check` over what was left. A log the crash left behind is
//!    part of a consistent volume and not a fault.
//! 3. `ferrix.btrfs=replay`: the guest mounts the disk again, which replays
//!    the log, checks every file whose trailer says its body was promised
//!    (`src/kernel/src/fs/btrfs_powerfail.rs` says why that is a sound thing to
//!    ask), and unmounts.
//! 4. Host `btrfs check` again, which must find nothing.
//!
//! What a QEMU kill can and cannot do: QEMU writes what the guest writes into
//! the host's page cache, and a killed QEMU loses only what it had not yet
//! passed on. So this is a real driver, block ring and host under the write
//! path's ordering, cut at a real moment, but it is gentler than a power
//! failure, which may lose any write not yet flushed. The adversarial version
//! of the same test — every write after the last flush kept or dropped at
//! random, thousands of cuts — is `src/lib/fs/btrfs-write`'s `powerfail` tests.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::btrfs_check::Checker;
use crate::paths::Arch;
use crate::{Error, Result, btrfs_disk, qemu};

/// What the churn prints once it is writing.
const CHURNING: &str = "btrfs-pf churning on vdc";

/// What the replay prints just before it mounts: the disk is untouched
/// until this line, so a boot that died before it may be tried again.
const REPLAYING: &str = "btrfs-pf replaying vdc";

/// What the replay prints once it has mounted, judged and unmounted.
const REPLAYED: &str = "btrfs-pf vdc replayed";

/// What the guest prints when it has no writable disk.
const SKIPPED: &str = "btrfs-pf not run";

/// What the replay adds when the crash left a log and the mount replayed it.
const REPLAYED_LOG: &str = "the crash left a log, and the mount replayed it";

/// From this many seeds on, at least one cut must have left a log: a run
/// whose every cut fell between a commit and the next log commit tested the
/// commit and never the replay.
const SEEDS_THAT_MUST_LOG: u64 = 4;

/// The longest the churn runs before the power goes.
const LONGEST: Duration = Duration::from_millis(4000);

/// The halves and the archive an image is made of, built once per
/// architecture: each boot needs its own command line, and so its own image,
/// but not its own compile.
#[derive(Debug)]
pub(crate) struct Built {
    pub(crate) loader: PathBuf,
    pub(crate) kernel: crate::cargo::Kernel,
    pub(crate) initramfs: Vec<u8>,
}

/// Run `args.seeds` power failures on each architecture asked for.
///
/// # Errors
///
/// A boot that did not start churning, a guest check that failed, a missing
/// btrfs-progs, or anything `btrfs check` said.
pub(crate) fn test_powerfail(args: &Args, build: impl Fn(Arch) -> Result<Built>) -> Result<()> {
    let checker = Checker::required()?;
    for arch in args.arches()? {
        let built = build(arch)?;
        let mut logged = 0u64;
        for seed in 1..=args.seeds {
            println!("  {arch}: power failure {seed} of {}", args.seeds);
            // Every seed starts from a fresh volume.
            btrfs_disk::keep_blank(false);
            let replayed = retried(arch, seed, "churn", CHURNING, || {
                churn(arch, &built, args, seed)
            })
            .and_then(|()| checker.run(&btrfs_disk::blank_path(arch), arch))
            .and_then(|()| {
                btrfs_disk::keep_blank(true);
                retried(arch, seed, "replay", REPLAYING, || {
                    replay(arch, &built, args, &checker)
                })
            });
            btrfs_disk::keep_blank(false);
            if replayed? {
                logged += 1;
            }
        }
        println!(
            "  {arch}: {} power failures, {logged} of them leaving a log the next mount \
             replayed, and btrfs check passed every volume before and after",
            args.seeds
        );
        if logged == 0 && args.seeds >= SEEDS_THAT_MUST_LOG {
            return Err(Error::new(format!(
                "{arch}: not one of {} cuts left a log, so the replay was never tested",
                args.seeds
            )));
        }
    }
    Ok(())
}

/// The first boot of a seed: churn on a fresh volume, and cut the power.
fn churn(arch: Arch, built: &Built, args: &Args, seed: u64) -> Result<()> {
    let image = boot_image(
        arch,
        built,
        &format!("ferrix.btrfs=churn ferrix.btrfs.seed={seed}"),
    )?;
    let delay = LONGEST.mul_f64(fraction(seed));
    let _ = qemu::watch_then(arch, &image, &built.kernel, args, CHURNING, |watching| {
        // Read what the guest says while it churns, so a write path that
        // fails before the cut fails the test rather than being cut short.
        let failed = |lines: &[String]| {
            lines.iter().any(|line| {
                line.contains(qemu::PANIC_MARKER) || line.contains("power-fail check failed")
            })
        };
        if watching.read_more(Instant::now() + delay, failed)? {
            return Err(Error::new(format!(
                "{arch}: seed {seed}: the churn failed before the power was cut"
            )));
        }
        println!(
            "  {arch}: seed {seed}: power cut {} ms into the churn",
            delay.as_millis()
        );
        watching.cut_power();
        Ok(())
    })?;
    Ok(())
}

/// The second boot of a seed, on the disk the cut left: replay, then check
/// again. Answers whether the cut left a log for the replay.
fn replay(arch: Arch, built: &Built, args: &Args, checker: &Checker) -> Result<bool> {
    let disk = btrfs_disk::blank_path(arch);
    let image = boot_image(arch, built, "ferrix.btrfs=replay")?;
    let lines = qemu::watch_then(arch, &image, &built.kernel, args, REPLAYED, |_| Ok(()))?;
    if lines.iter().any(|line| line.contains(SKIPPED)) {
        return Err(Error::new(format!(
            "{arch}: the replay boot had no writable disk, so nothing was replayed"
        )));
    }
    checker.run(&disk, arch)?;
    Ok(lines.iter().any(|line| line.contains(REPLAYED_LOG)))
}

/// Run one boot of a seed, and run it once more if it failed before it
/// printed `reached` — in a check of another stage, before this one touched
/// the disk, which says nothing about stage 12. Said, and the log kept; a
/// second failure is a failure. FX-0701 is why.
fn retried<T>(
    arch: Arch,
    seed: u64,
    what: &str,
    reached: &str,
    mut boot: impl FnMut() -> Result<T>,
) -> Result<T> {
    let first = boot();
    // Read straight after the boot, whose log it still is.
    let started = std::fs::read_to_string(serial_log(arch)).is_ok_and(|log| log.contains(reached));
    if first.is_ok() || started {
        return first;
    }
    let kept = keep_log(arch, seed, what)?;
    println!(
        "  {arch}: seed {seed}: the {what} boot failed before stage 12 began, in a check \
         that is not stage 12's; its log is kept at {}, and it runs once more",
        kept.display()
    );
    boot()
}

/// The serial log every watched boot writes.
fn serial_log(arch: Arch) -> PathBuf {
    crate::paths::build_dir(arch).join("serial.log")
}

/// Copy the last boot's serial log where the next boot will not overwrite it.
fn keep_log(arch: Arch, seed: u64, what: &str) -> Result<PathBuf> {
    let kept = crate::paths::build_dir(arch).join(format!("powerfail-seed{seed}-{what}.log"));
    let _ = std::fs::copy(serial_log(arch), &kept)?;
    Ok(kept)
}

/// The image for one boot, carrying `cmdline`.
fn boot_image(arch: Arch, built: &Built, cmdline: &str) -> Result<PathBuf> {
    crate::fat::write_image_with(
        arch,
        &built.loader,
        &built.kernel,
        &built.initramfs,
        Some(&format!("{cmdline}\n")),
    )
}

/// A number in `[0, 1)` the seed picks, the same every run.
fn fraction(seed: u64) -> f64 {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    let mixed = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
    (mixed >> 11) as f64 / (1u64 << 53) as f64
}
