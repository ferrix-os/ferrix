//! `cargo xtask test-mce-restart`: a fatal machine check, the warm reset
//! `ferrix.panic=reset` makes, and the next boot keeping the bad frame out
//! from its start (`docs/MACHINE-CHECK-RESTART.md` §5, slice 8a).
//!
//! One QEMU kept across the reset (no `-no-reboot`, RAM kept), Intel's
//! records under TCG, one processor, test-mce's victim as init. After the
//! victim's line the gate stops the machine until the processor runs the
//! victim in ring 3 with its page mapped, injects a fatal record (`PCC`) at
//! that page, and lets it go: the kernel stops as FX-9005 (its stop word 8,
//! the page in the pstore bad-frame table) and resets itself after the
//! report's wait. Boot 2's loader must say boot 1 ended in a machine check
//! and keep the page out, from the banks (`left`) and from the table; boot
//! 2's kernel must leave it out and list it retired; its poll must print the
//! left record and find the frame already retired. The restart's time, from
//! the report to boot 2's poll, is said with its parts.
//!
//! Boot 2 is judged up to its poll's lines and not to `FERRIX-BOOT-OK`:
//! stage 12's write check needs the fresh writable btrfs disk xtask makes for
//! each QEMU start, and a warm reset keeps the disk boot 1 wrote.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{INTEL, gpa, in_ring_3, monitor, refusal, stopped};
use crate::paths::Arch;
use crate::task::Task;
use crate::{Error, Result, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-mce-restart",
        help_at: 258,
        help: &[
            "test-mce-restart  A fatal machine check under ferrix.panic=reset: the next boot keeps the bad frame out from",
            "                  its start (the pstore table and the banks), and the restart's time is said",
        ],
        run: |_, args| test(args),
    },
    Task {
        name: "test-mce-restart-retired",
        help_at: 259,
        help: &[
            "test-mce-restart-retired  A frame a recovered machine check retired: kept out of boot 2 from the table",
            "                          retire_frame wrote, and of boot 3 from the table boot 2's attach wrote again",
        ],
        run: |_, args| test_retired(args),
    },
    Task {
        name: "test-mce-restart-power",
        help_at: 261,
        help: &[
            "test-mce-restart-power  A retired frame kept in the event log's bad-frame set: kept out of a cold boot",
            "                        after a power cut, nothing written on later boots, gone after the host's clear",
        ],
        run: |_, args| test_power(args),
    },
];

/// The guest program the boot runs as init (`src/tests/mce`).
const TEST_PROGRAM: &str = "mce";
/// The victim's line: `mce-test: victim pid=<pid> va=<address>`.
const VICTIM: &str = "mce-test: victim pid=";
/// The report's line before the reset's wait.
const RESET_LINE: &str = "  reset     in 10 s, as ferrix.panic=reset asks";
/// The loader's banner, which begins boot 2.
const LOADER_BANNER: &str = "Ferrix loader ";
/// The poll's start line, boot 2's: the scheduler and the poll tasks run.
const POLLING: &str = "mce: polling on ";
/// The poll's finding of the left record's frame, boot 2's last line judged.
const ALREADY: &str = "already retired";
/// `MCi_STATUS` of a fatal data load: `VAL UC EN PCC MISCV ADDRV`, MCA code
/// 0x134.
const FATAL: u64 = 0xbe00_0000_0000_0134;
/// `MCi_STATUS` of an action-required data load: `VAL UC EN MISCV ADDRV S
/// AR`, MCA code 0x134; recovered under TCG, whose Intel model reports
/// `MCG_SER_P`.
const SRAR: u64 = 0xbd80_0000_0000_0134;
/// The guest's last line, boot 1's.
const DONE: &str = "mce-test: done";
/// The kernel's last line as it powers off, which pauses a QEMU that keeps
/// RAM.
const POWERED_OFF: &str = "FERRIX-POWER-OFF";
/// `MCG_STATUS`: `RIPV EIPV MCIP`.
const CONSUMER: u64 = 0x7;
/// `MCi_MISC`: a physical address, recoverable to the page.
const MISC_PAGE: u64 = 0x8c;
/// The bank injected into.
const BANK: u32 = 1;
/// How long each step is given.
const PATIENCE: Duration = Duration::from_secs(150);

/// What the hook saw, for the judge.
#[derive(Default)]
struct Seen {
    /// The page injected at.
    page: u64,
    /// Boot 2's lines, from its loader's banner.
    after: Vec<String>,
    /// From the report's reset line to boot 2's loader banner.
    to_loader: Duration,
    /// From boot 2's loader banner to its poll's start.
    to_boot: Duration,
}

/// The test.
///
/// # Errors
///
/// A boot that did not reach the victim, no ring-3 stop at its page, an
/// injection the monitor refused, or a line the judge wants missing.
///
/// Verifies: `H.FAIL.12`
/// Verifies: `L.console.58`
/// Verifies: `L.mm.109`
/// Verifies: `L.mm.110`
/// Verifies: `L.x86_64.254`
fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-restart: x86_64 only; skipped");
        return Ok(());
    }
    let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let options = "ferrix.mce.poll=1 ferrix.panic=reset nokaslr\n";
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.keep_ram = true;
    qemu_args.accel = Some("tcg".to_owned());
    qemu_args.x86_cpu_extra = Some(INTEL.to_owned());
    qemu_args.smp = 1;
    qemu_args.smp_given = true;
    if !args.timeout_given {
        qemu_args.timeout = 420;
    }
    let mut seen = Seen::default();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        seen.page = inject(&mut qmp, watching.lines(), FATAL)?;
        if !watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(RESET_LINE))
        })? {
            return Err(Error::new(format!(
                "the stop's report never said `{}`",
                RESET_LINE.trim()
            )));
        }
        let reported = Instant::now();
        let from = watching.after().len();
        let banner_seen = watching.read_more(reported + PATIENCE, |lines| {
            lines
                .get(from..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(LOADER_BANNER))
        })?;
        if !banner_seen {
            return Err(Error::new("the machine did not reset after the stop"));
        }
        seen.to_loader = reported.elapsed();
        let banner = watching
            .after()
            .iter()
            .skip(from)
            .position(|line| line.contains(LOADER_BANNER))
            .map_or(from, |at| from + at);
        let started = Instant::now();
        let _ = watching.read_more(started + PATIENCE, |lines| {
            lines
                .get(banner..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(POLLING))
        })?;
        seen.to_boot = started.elapsed();
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines
                .get(banner..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(ALREADY) || line.contains(qemu::PANIC_MARKER))
        })?;
        seen.after = watching.after().get(banner..).unwrap_or_default().to_vec();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &image, &kernel, &qemu_args, VICTIM, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    judge(&seen).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-restart: {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-restart: as required (frame {:#x} kept out of boot 2 from the table and \
         the banks; TCG, Intel's records)",
        seen.page
    );
    println!(
        "  {arch}: test-mce-restart: the restart took {:.1} s from the report to boot 2's poll: \
         {:.1} s to its loader (the report's 10 s wait, firmware), {:.1} s of loader and boot to \
         the poll's start",
        (seen.to_loader + seen.to_boot).as_secs_f64(),
        seen.to_loader.as_secs_f64(),
        seen.to_boot.as_secs_f64()
    );
    Ok(())
}

/// Stop until the processor runs the victim in ring 3 with its page mapped,
/// inject a record of `status` there, and go on; the page.
fn inject(qmp: &mut Qmp, lines: &[String], status: u64) -> Result<u64> {
    let virt = lines
        .iter()
        .rev()
        .find_map(|line| {
            let at = line.find(" va=0x")? + " va=0x".len();
            u64::from_str_radix(line.get(at..)?.trim(), 16).ok()
        })
        .ok_or_else(|| Error::new("the victim printed no address"))?;
    for _ in 0..400 {
        stopped(qmp, true)?;
        let mut page = None;
        if in_ring_3(&monitor(qmp, "info registers", Some(0))?) {
            page = gpa(&monitor(qmp, &format!("gva2gpa {virt:#x}"), Some(0))?)
                .map(|physical| physical & !0xfff);
        }
        let made = page.map(|page| {
            let command =
                format!("mce 0 {BANK} {status:#x} {CONSUMER:#x} {page:#x} {MISC_PAGE:#x}");
            let reply = monitor(qmp, &command, None)?;
            if let Some(problem) = refusal(&reply) {
                return Err(Error::new(format!(
                    "the monitor did not inject `{command}`: {problem}"
                )));
            }
            println!("  x86_64: injected `{command}`");
            Ok(page)
        });
        stopped(qmp, false)?;
        if let Some(made) = made {
            return made;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(Error::new(format!(
        "no stop found the victim in ring 3 with {virt:#x} mapped within 400 tries"
    )))
}

/// Whether boot 2's lines show what the slice requires.
fn judge(seen: &Seen) -> std::result::Result<(), String> {
    let has = |want: &str| seen.after.iter().any(|line| line.contains(want));
    let page = format!("{:#x}", seen.page);
    for (want, missing) in [
        (
            "pstore   boot 1 ended: a machine check".to_owned(),
            "boot 2's loader did not say boot 1 ended in a machine check",
        ),
        (
            format!("badframe {page} kept out (left)"),
            "boot 2's loader did not keep the banks' page out",
        ),
        (
            format!("badframe {page} kept out (table"),
            "boot 2's loader did not find the page in the pstore table",
        ),
        (
            "mm       1 frames left out as bad".to_owned(),
            "boot 2's kernel did not leave the frame out",
        ),
        (
            "left from before this boot".to_owned(),
            "boot 2's poll did not print the left record",
        ),
        (
            format!("mce: frame {page} already retired"),
            "boot 2's poll did not find the frame already retired",
        ),
    ] {
        if !has(&want) {
            return Err(format!("{missing}: no `{want}`"));
        }
    }
    if has(qemu::PANIC_MARKER) {
        return Err("boot 2 stopped".to_owned());
    }
    Ok(())
}

/// A frame a recovered machine check retired, through `retire_frame`
/// (`L.mm.109`), kept out across two warm resets (`L.mm.110`; ledger 5487
/// (2)). Boot 1 recovers an action-required record at the victim's page --
/// the frame retired, the bank cleared, so nothing is left there -- and once
/// its init has exited and the machine powered off (which only pauses a
/// QEMU that keeps RAM) the gate resets it over QMP, a warm reset with RAM
/// kept, as `test-postmortem` resets a powered-off boot. Boot 2's loader
/// must keep the frame out from the table alone (`kept out (table)`, not
/// `as already`, and no `(left)`), and its kernel leave it out; boot 2 then
/// stops in stage 12's write check on the disk boot 1 wrote, which
/// `ferrix.panic=reset` turns into the second reset; boot 3's loader must
/// keep the frame out again, from the table boot 2's attach wrote.
///
/// # Errors
///
/// As [`test()`].
///
/// Verifies: `H.FAIL.12`
/// Verifies: `L.mm.109`
/// Verifies: `L.mm.110`
fn test_retired(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-restart (Retired): x86_64 only; skipped");
        return Ok(());
    }
    let (image, kernel) = retired_image(arch, args)?;
    let port = free_port()?;
    let qemu_args = retired_args(args, port);
    let mut page = 0;
    let mut boots: Vec<Vec<String>> = Vec::new();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        page = inject(&mut qmp, watching.lines(), SRAR)?;
        let kept = format!("badframe {page:#x} kept out");
        // Boot 1 to its power-off, then the warm reset from outside.
        if !watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(POWERED_OFF))
        })? {
            return Err(Error::new("boot 1 did not finish and power off"));
        }
        if !crate::postmortem::paused(&mut qmp, Instant::now() + Duration::from_secs(30))? {
            return Err(Error::new("QEMU did not pause at boot 1's power-off"));
        }
        let _ = qmp.execute("system_reset", None)?;
        // A paused machine stays paused across the reset.
        let _ = qmp.execute("cont", None);
        // Boot 1 to its init's exit and reset; boots 2 and 3 to their
        // loaders' line for the frame, or past where it would be.
        for boot in 2..=3 {
            let from = watching.after().len();
            let started = watching.read_more(Instant::now() + PATIENCE * 2, |lines| {
                lines
                    .get(from..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(LOADER_BANNER))
            })?;
            if !started {
                return Err(Error::new(format!("boot {boot} never started")));
            }
            let banner = watching
                .after()
                .iter()
                .skip(from)
                .position(|line| line.contains(LOADER_BANNER))
                .map_or(from, |at| from + at);
            let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
                lines
                    .get(banner..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(POLLING))
            })?;
            boots.push(watching.after().get(banner..).unwrap_or_default().to_vec());
            if boot == 2
                && !boots
                    .first()
                    .is_some_and(|lines| lines.iter().any(|line| line.contains(&kept)))
            {
                break;
            }
        }
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, VICTIM, hook)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    judge_retired(&lines, page, &boots).map_err(|why| {
        Error::new(format!(
            "{arch}: test-mce-restart (Retired): {why}.\n  Serial output is in {}",
            log.display()
        ))
    })?;
    println!(
        "  {arch}: test-mce-restart (Retired): as required (frame {page:#x} retired by a recovered \
         machine check, kept out of boots 2 and 3 from the table; TCG, Intel's records)"
    );
    Ok(())
}

/// The image the recovered-retirement modes boot: test-mce as init, polled
/// banks, `ferrix.panic=reset`, no KASLR.
fn retired_image(arch: Arch, args: &Args) -> Result<(std::path::PathBuf, crate::cargo::Kernel)> {
    let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel =
        crate::cargo::build_kernel_with_init(arch, args.release, &program, crate::shell::SCRIPT)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &[])?;
    let options = "ferrix.mce.poll=1 ferrix.panic=reset nokaslr\n";
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(options))?;
    Ok((image, kernel))
}

/// QEMU's arguments for those modes: QMP on `port`, RAM kept across a
/// reset, TCG with Intel's records, one processor.
fn retired_args(args: &Args, port: u16) -> Args {
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    qemu_args.keep_ram = true;
    qemu_args.accel = Some("tcg".to_owned());
    qemu_args.x86_cpu_extra = Some(INTEL.to_owned());
    qemu_args.smp = 1;
    qemu_args.smp_given = true;
    if !args.timeout_given {
        qemu_args.timeout = 600;
    }
    qemu_args
}

/// Wait for the next loader banner after line `from` of the lines read
/// since, then for the boot's poll, or past where it would be; the boot's
/// lines from its banner. `None` when no banner came.
fn next_boot(watching: &mut qemu::Watching<'_>, from: usize) -> Result<Option<Vec<String>>> {
    let started = watching.read_more(Instant::now() + PATIENCE * 2, |lines| {
        lines
            .get(from..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains(LOADER_BANNER))
    })?;
    if !started {
        return Ok(None);
    }
    let banner = watching
        .after()
        .iter()
        .skip(from)
        .position(|line| line.contains(LOADER_BANNER))
        .map_or(from, |at| from + at);
    let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
        lines
            .get(banner..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.contains(POLLING))
    })?;
    Ok(Some(
        watching.after().get(banner..).unwrap_or_default().to_vec(),
    ))
}

/// The bad-frame set's generation and frames on the test disk's event log:
/// what the host's `ferrix-eventlog list` reads.
fn log_set(disk: &std::path::Path) -> Result<(u64, Vec<u32>)> {
    use ferrix_eventlog::{self as eventlog, Current};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)?;
    let (mut device, geometry) = crate::postmortem::log_device(&file)?;
    match eventlog::read_header(&mut device, &geometry) {
        Ok(Current::Header(header)) => Ok((header.sequence, header.bad.frames().to_vec())),
        Ok(Current::Fresh) => Ok((0, Vec::new())),
        Ok(Current::Damaged) => Err(Error::new("the event log's header is damaged")),
        Err(why) => Err(Error::new(format!("the event log's header: {why:?}"))),
    }
}

/// The host's `ferrix-eventlog clear-bad-frames` on the test disk.
fn clear_set(disk: &std::path::Path) -> Result<Vec<u32>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(disk)?;
    let (mut device, geometry) = crate::postmortem::log_device(&file)?;
    ferrix_eventlog::clear_bad_frames(&mut device, &geometry)
        .map(|dropped| dropped.frames().to_vec())
        .map_err(|why| Error::new(format!("the clear: {why:?}")))
}

/// Attempts `PowerCycle` makes at most (ledger 5564 (2)).
const POWER_ATTEMPTS: u32 = 3;

/// `PowerCycle` (`docs/MACHINE-CHECK-RESTART.md` §9.6): the disk in a GUID
/// partition table with the event log, as `test-postmortem` builds it. Boot
/// 1 recovers an SRAR and retires the victim's frame and powers off; a warm
/// reset from outside; boot 2's loader keeps the frame out from the table
/// and keeps it in the event log; the QEMU process is ended (the power cut).
/// A second process boots cold: boot 3's loader keeps the frame out from the
/// log alone, and a warm boot 4 after it writes nothing to the log. The host
/// reads the set (the frame) and clears it; a third process's cold boot 5
/// keeps nothing out. TCG only.
///
/// An attempt whose victim page firmware holds by the time the loader reads
/// the table (boot 2) or the log (boot 3) is inconclusive, said, and made
/// again with a fresh disk and a fresh injection, at most
/// [`POWER_ATTEMPTS`] in all; every attempt inconclusive fails (AoU-39's
/// order residual, ledger 5564).
///
/// # Errors
///
/// As [`test()`], and every attempt inconclusive.
///
/// Verifies: `H.FAIL.13`
/// Verifies: `L.mm.110`
fn test_power(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-mce-restart (PowerCycle): x86_64 only; skipped");
        return Ok(());
    }
    let (image, kernel) = retired_image(arch, args)?;
    let log = crate::paths::build_dir(arch).join("serial.log");
    let failed = |why: String| {
        Error::new(format!(
            "{arch}: test-mce-restart (PowerCycle): {why}.\n  Serial output is in {}",
            log.display()
        ))
    };
    let mut inconclusive = 0;
    for attempt in 1..=POWER_ATTEMPTS {
        match power_attempt(arch, args, &image, &kernel)? {
            Attempt::Passed(page) => {
                println!(
                    "  {arch}: test-mce-restart (PowerCycle): as required (frame {page:#x} kept in \
                     the event log, kept out of a cold boot after a power cut, nothing written \
                     after, cleared by the host; {inconclusive} of {attempt} attempts inconclusive \
                     (firmware held the page); TCG, Intel's records)"
                );
                return Ok(());
            }
            Attempt::Failed(why) => return Err(failed(why)),
            Attempt::Inconclusive { page, boot } => {
                inconclusive += 1;
                println!(
                    "  {arch}: test-mce-restart (PowerCycle): attempt {attempt} inconclusive: \
                     firmware held frame {page:#x} at boot {boot}'s loader; again with a fresh \
                     disk and injection"
                );
            }
        }
    }
    Err(failed(format!(
        "all {POWER_ATTEMPTS} attempts inconclusive: firmware held every victim page"
    )))
}

/// How one `PowerCycle` attempt ended.
enum Attempt {
    /// Every boot as required, the frame injected at this page.
    Passed(u64),
    /// Firmware held the page when boot `boot`'s loader read it.
    Inconclusive { page: u64, boot: u32 },
    /// A line the judge wants missing.
    Failed(String),
}

/// One `PowerCycle` attempt, from a fresh disk.
fn power_attempt(
    arch: Arch,
    args: &Args,
    image: &std::path::Path,
    kernel: &crate::cargo::Kernel,
) -> Result<Attempt> {
    let disk = crate::paths::build_dir(arch).join("mce-restart-disk.img");
    crate::postmortem::write_disk(image, &disk)?;
    let port = free_port()?;
    let qemu_args = retired_args(args, port);
    let mut page = 0;
    let mut boot_2 = Vec::new();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        page = inject(&mut qmp, watching.lines(), SRAR)?;
        if !watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(POWERED_OFF))
        })? {
            return Err(Error::new("boot 1 did not finish and power off"));
        }
        if !crate::postmortem::paused(&mut qmp, Instant::now() + Duration::from_secs(30))? {
            return Err(Error::new("QEMU did not pause at boot 1's power-off"));
        }
        let _ = qmp.execute("system_reset", None)?;
        let _ = qmp.execute("cont", None);
        let from = watching.after().len();
        boot_2 = next_boot(watching, from)?.ok_or_else(|| Error::new("boot 2 never started"))?;
        // The power cut: the process ends, and RAM with it.
        watching.cut_power();
        Ok(())
    };
    let boot_1 = qemu::watch_then(arch, &disk, kernel, &qemu_args, VICTIM, hook)?;
    match after_boot_2(&boot_1, &boot_2, page) {
        Step::Go => {}
        Step::Inconclusive => return Ok(Attempt::Inconclusive { page, boot: 2 }),
        Step::Fail(why) => return Ok(Attempt::Failed(why)),
    }
    let after_2 = log_set(&disk)?;
    // Boot 3, cold, and a warm boot 4 from inside the same process.
    let port = free_port()?;
    let cold_args = retired_args(args, port);
    let mut boot_4 = Vec::new();
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(POLLING))
        })?;
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let from = watching.after().len();
        let _ = qmp.execute("system_reset", None)?;
        boot_4 = next_boot(watching, from)?.ok_or_else(|| Error::new("boot 4 never started"))?;
        watching.cut_power();
        Ok(())
    };
    let boot_3 = qemu::watch_then(arch, &disk, kernel, &cold_args, LOADER_BANNER, hook)?;
    match after_boot_3(&boot_3, page) {
        Step::Go => {}
        Step::Inconclusive => return Ok(Attempt::Inconclusive { page, boot: 3 }),
        Step::Fail(why) => return Ok(Attempt::Failed(why)),
    }
    let after_4 = log_set(&disk)?;
    let dropped = clear_set(&disk)?;
    // Boot 5, cold, after the clear.
    let port = free_port()?;
    let cold_args = retired_args(args, port);
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(POLLING))
        })?;
        watching.cut_power();
        Ok(())
    };
    let boot_5 = qemu::watch_then(arch, &disk, kernel, &cold_args, LOADER_BANNER, hook)?;
    let later = LaterBoots {
        boot_4: &boot_4,
        boot_5: &boot_5,
    };
    Ok(
        match judge_power(&later, page, &after_2, &after_4, &dropped) {
            Ok(()) => Attempt::Passed(page),
            Err(why) => Attempt::Failed(why),
        },
    )
}

/// What a boot's lines say of an attempt so far (ledger 5564 (1)).
#[derive(Debug, PartialEq, Eq)]
enum Step {
    /// As required: on to the next boot.
    Go,
    /// Firmware held the victim page when the loader read it, and nothing
    /// else is wrong: the attempt proves nothing either way.
    Inconclusive,
    /// A line the judge wants missing.
    Fail(String),
}

/// Whether a boot's lines hold `want`.
fn has(lines: &[String], want: &str) -> bool {
    lines.iter().any(|line| line.contains(want))
}

/// Boots 1 and 2: boot 1 retired the frame; boot 2's loader kept it out from
/// the table and kept it in the event log. Inconclusive only when the loader
/// says exactly that the page is in use and still kept it in the log.
fn after_boot_2(boot_1: &[String], boot_2: &[String], page: u64) -> Step {
    if !has(boot_1, &format!("mce: frame {page:#x} retired")) {
        return Step::Fail(format!("boot 1 did not retire {page:#x}"));
    }
    let logged = format!("events   bad frame {page:#x} kept in the event log (1 of 64)");
    let logged_ok = has(boot_2, &logged);
    let table = format!("badframe {page:#x} kept out (table)");
    let in_use = format!("badframe {page:#x} could not be kept out (table): it is in use");
    if has(boot_2, &table) {
        if logged_ok {
            Step::Go
        } else {
            Step::Fail(format!(
                "boot 2's loader did not keep the frame in the event log: no `{logged}`"
            ))
        }
    } else if has(boot_2, &in_use) && logged_ok {
        Step::Inconclusive
    } else {
        Step::Fail(format!(
            "boot 2's loader did not keep the frame out from the table and in the log: no \
             `{table}` and `{logged}`"
        ))
    }
}

/// Boot 3, cold: its loader kept the frame out from the log alone and its
/// kernel left it out. Inconclusive only when the loader says exactly that
/// the page is in use and still read the set (its summary line).
fn after_boot_3(boot_3: &[String], page: u64) -> Step {
    let from_log = format!("badframe {page:#x} kept out (log)");
    let in_use = format!("badframe {page:#x} could not be kept out (log): it is in use");
    let summary = "events   1 bad frame(s) kept out across power cycles";
    if has(boot_3, &from_log) {
        if has(boot_3, &format!("badframe {page:#x} kept out (table)"))
            || has(boot_3, &format!("badframe {page:#x} kept out (left)"))
        {
            Step::Fail("boot 3, cold, found the frame in the table or the banks".to_owned())
        } else if has(boot_3, "mm       1 frames left out as bad") {
            Step::Go
        } else {
            Step::Fail("boot 3's kernel did not leave the frame out".to_owned())
        }
    } else if has(boot_3, &in_use) && has(boot_3, summary) {
        Step::Inconclusive
    } else {
        Step::Fail(format!(
            "boot 3's loader did not keep the frame out from the event log: no `{from_log}`"
        ))
    }
}

/// The boots after the cold one that `judge_power` reads.
struct LaterBoots<'a> {
    boot_4: &'a [String],
    boot_5: &'a [String],
}

/// Past boot 3: the log's set held the frame after boot 2, boots 3 and 4
/// wrote nothing, the host's clear dropped the frame, and boot 5 kept
/// nothing out.
fn judge_power(
    boots: &LaterBoots<'_>,
    page: u64,
    after_2: &(u64, Vec<u32>),
    after_4: &(u64, Vec<u32>),
    dropped: &[u32],
) -> std::result::Result<(), String> {
    let frame = u32::try_from(page / 4096).unwrap_or(u32::MAX);
    if after_2.1 != [frame] {
        return Err(format!(
            "after boot 2 the log's set is {:?}, not [{frame:#x}]",
            after_2.1
        ));
    }
    if !has(boots.boot_4, &format!("badframe {page:#x} kept out (log)")) {
        return Err("boot 4's loader did not keep the frame out from the event log".to_owned());
    }
    if after_4 != after_2 {
        return Err(format!(
            "boots 3 and 4 wrote the log: generation {} then {}",
            after_2.0, after_4.0
        ));
    }
    if dropped != [frame] {
        return Err(format!(
            "the host's clear dropped {dropped:?}, not [{frame:#x}]"
        ));
    }
    if has(boots.boot_5, "kept out (log)") || has(boots.boot_5, "frames left out as bad") {
        return Err("boot 5, after the clear, still kept a frame out".to_owned());
    }
    if !has(boots.boot_5, POLLING) {
        return Err("boot 5 did not reach its poll".to_owned());
    }
    Ok(())
}

/// Whether boot 1 recovered and boots 2 and 3 kept the frame out from the
/// table alone.
fn judge_retired(
    boot_1: &[String],
    page: u64,
    boots: &[Vec<String>],
) -> std::result::Result<(), String> {
    let retired = format!("mce: frame {page:#x} retired");
    if !boot_1.iter().any(|line| line.contains(&retired))
        || !boot_1.iter().any(|line| line.contains(DONE))
    {
        return Err(format!("boot 1 did not retire {page:#x} and finish"));
    }
    let table = format!("badframe {page:#x} kept out (table)");
    let left = format!("badframe {page:#x} kept out (left)");
    for (index, lines) in boots.iter().enumerate() {
        let boot = index + 2;
        let has = |want: &str| lines.iter().any(|line| line.contains(want));
        if !has(&table) {
            return Err(format!(
                "boot {boot}'s loader did not keep the frame out from the table: no `{table}`"
            ));
        }
        if has(&left) {
            return Err(format!(
                "boot {boot}'s loader found the frame in the banks, which recovery cleared"
            ));
        }
        if !has("mm       1 frames left out as bad") {
            return Err(format!("boot {boot}'s kernel did not leave the frame out"));
        }
    }
    if boots.len() != 2 {
        return Err(format!("{} restarted boots seen, not 2", boots.len()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Step, after_boot_2, after_boot_3};

    const PAGE: u64 = 0x1e28_6000;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    fn boot_1() -> Vec<String> {
        lines(&["mce: frame 0x1e286000 retired"])
    }

    const LOGGED: &str = "  events   bad frame 0x1e286000 kept in the event log (1 of 64)";

    /// Verifies the classifier's pass (ledger 5564 (1)).
    #[test]
    fn a_kept_out_page_goes_on() {
        let boot_2 = lines(&["  badframe 0x1e286000 kept out (table)", LOGGED]);
        assert_eq!(after_boot_2(&boot_1(), &boot_2, PAGE), Step::Go);
        let boot_3 = lines(&[
            "  badframe 0x1e286000 kept out (log)",
            "  events   1 bad frame(s) kept out across power cycles",
            "  mm       1 frames left out as bad",
        ]);
        assert_eq!(after_boot_3(&boot_3, PAGE), Step::Go);
    }

    /// Verifies that a page firmware held at the table's read is
    /// inconclusive, as m8b-d10 was.
    #[test]
    fn a_table_page_in_use_is_inconclusive() {
        let boot_2 = lines(&[
            "  badframe 0x1e286000 could not be kept out (table): it is in use (firmware or the \
             loader's earlier allocation)",
            LOGGED,
        ]);
        assert_eq!(after_boot_2(&boot_1(), &boot_2, PAGE), Step::Inconclusive);
    }

    /// Verifies that a page firmware held at the log's read is inconclusive.
    #[test]
    fn a_log_page_in_use_is_inconclusive() {
        let boot_3 = lines(&[
            "  badframe 0x1e286000 could not be kept out (log): it is in use (firmware or the \
             loader's earlier allocation)",
            "  events   1 bad frame(s) kept out across power cycles",
        ]);
        assert_eq!(after_boot_3(&boot_3, PAGE), Step::Inconclusive);
    }

    /// Verifies that a page in use whose frame never reached the log fails.
    #[test]
    fn in_use_without_the_event_log_line_fails() {
        let boot_2 = lines(&[
            "  badframe 0x1e286000 could not be kept out (table): it is in use (firmware or the \
             loader's earlier allocation)",
        ]);
        assert!(matches!(
            after_boot_2(&boot_1(), &boot_2, PAGE),
            Step::Fail(_)
        ));
        let boot_3 = lines(&[
            "  badframe 0x1e286000 could not be kept out (log): it is in use (firmware or the \
             loader's earlier allocation)",
        ]);
        assert!(matches!(after_boot_3(&boot_3, PAGE), Step::Fail(_)));
    }

    /// Verifies that another page's in-use line proves nothing for this one.
    #[test]
    fn another_page_in_use_fails() {
        let boot_2 = lines(&[
            "  badframe 0x1000 could not be kept out (table): it is in use (firmware or the \
             loader's earlier allocation)",
            LOGGED,
        ]);
        assert!(matches!(
            after_boot_2(&boot_1(), &boot_2, PAGE),
            Step::Fail(_)
        ));
    }

    /// Verifies that no badframe line at all fails.
    #[test]
    fn no_badframe_line_fails() {
        let boot_2 = lines(&[LOGGED]);
        assert!(matches!(
            after_boot_2(&boot_1(), &boot_2, PAGE),
            Step::Fail(_)
        ));
        assert!(matches!(after_boot_3(&lines(&[]), PAGE), Step::Fail(_)));
    }
}
