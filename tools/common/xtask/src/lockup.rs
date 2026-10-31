//! `cargo xtask test-lockup`: a hang ends in a report (`docs/POSTMORTEM.md`
//! §6.2 and §6.9). x86-64 and AArch64.
//!
//! **The lockup** (S4a, `ferrix.lockup=on`): the check masks the boot
//! processor and spins; its watcher must report it under FX-9007 within the
//! bound, and after a QMP reset the next loader must say the boot ended in a
//! lockup. **The quiet case**: every processor idle for 25 s with the
//! detector on, and no report.
//!
//! **The heartbeat** (S4c, `ferrix.watchdog=user`):
//!
//! 1. *stops*: busybox's `watchdog -F -T 5 -t 1` feeds `/dev/watchdog`,
//!    then is stopped with `SIGSTOP`; the report must come under FX-9008
//!    3.5 to 15 s later with a `cpu` line for every processor, the machine
//!    reset by itself (`ferrix.panic=reset`), the next loader say `boot 1
//!    ended: a lockup` and keep `LASTKERNEL.TXT` with the FX-9008 line;
//! 2. *killed*: the same feeder ended with `SIGKILL`, whose release holds
//!    no `V`: the watchdog stays armed and the report comes;
//! 3. *goes on*: the feeder fed for 15 s, a second open refused `EBUSY`,
//!    then `SIGTERM` (busybox writes `V` and closes, the magic close), a
//!    timeout of 0 refused `EINVAL`, and 10 s more: no report;
//! 4. *init feeds*: init as pid 1 with `ferrix.init.watchdog=5` for 20 s:
//!    init says it feeds, and no report;
//! 5. *off*: without `ferrix.watchdog`, `/dev/watchdog` is absent.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrix_bootinfo::pstore::Stop;

use crate::args::Args;
use crate::display::Qmp;
use crate::paths::Arch;
use crate::postmortem::{
    LOADER_BANNER, MIRRORING, PATIENCE, STOP_SCRIPT, WRITTEN_BACK, read,
};
use crate::qemu::{self, Watching};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-lockup",
    help_at: 501,
    help: &[
        "test-lockup   a hang ends in a report: a masked processor (ferrix.lockup=on), user space that stops feeding /dev/watchdog (ferrix.watchdog=user), and their quiet cases (x86_64, aarch64)",
    ],
    run: |_, args| test_lockup(args, crate::build_image),
}];

/// `cargo xtask test-lockup`, for each architecture asked.
pub(crate) fn test_lockup(
    args: &Args,
    build: impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            return Err(Error::new(
                "test-lockup runs on x86_64 and aarch64: the ARMv7-A board boots through \
                 U-Boot, whose RAM across a reset is not this gate's to claim",
            ));
        }
        locked_up(arch, args, &build)?;
        let busybox = crate::busybox::program(arch)?;
        let shell = read(&busybox)?;
        let loader = crate::cargo::build_loader(arch, args.release)?;
        let kernel = crate::cargo::build_kernel_with_init(arch, args.release, &busybox, "")?;
        let natives = crate::native::build(arch, args.release)?;
        let carried = crate::shell::carried_for(arch, &busybox, args)?;
        let parts = crate::init_file::Parts {
            arch,
            loader: &loader,
            kernel: &kernel,
            natives: &natives,
            program: &busybox,
            carried: &carried,
        };
        watched_quietly(arch, args, &parts, &shell)?;
        heartbeat_stops(arch, args, &parts, &shell, Signal::Stop)?;
        heartbeat_stops(arch, args, &parts, &shell, Signal::Kill)?;
        heartbeat_goes_on(arch, args, &parts, &shell)?;
        init_feeds(arch, args)?;
        heartbeat_off(arch, args, &parts, &shell)?;
        println!("  {arch}: test-lockup passed");
    }
    Ok(())
}

/// The report's code for user space that stopped feeding its watchdog.
const USER_LOCKUP_CODE: &str = "  code      FX-9008";
/// The kernel's line once the user watchdog is served.
const SERVED: &str = "  watchdog ferrix.watchdog=user: /dev/watchdog is served";
/// What the heartbeat scripts say.
const FEEDING: &str = "lk-heart: feeding";
const STOPPED: &str = "lk-heart: the feeder is stopped";
const SECOND_OPEN: &str = "lk-heart: second open done";
const MAGIC_SENT: &str = "lk-heart: magic close sent";
const STILL: &str = "lk-heart: still running";
/// busybox's words for the refusals.
const BUSY: &str = "Device or resource busy";
const INVALID: &str = "Invalid argument";
/// The kernel's line for the magic close.
const MAGIC_DISARMED: &str = "closed it with the magic `V`: disarmed";
/// A `cpu` line of a report.
const CPU_LINE: &str = "  cpu ";
/// The bounds of the heartbeat's report after the stop line: the timeout
/// less the feed period and the port's slack, and the timeout plus a tick
/// and a loaded host's delay (consultant W7).
const HEART_AT_LEAST: Duration = Duration::from_millis(3_500);
const HEART_AT_MOST: Duration = Duration::from_secs(15);
/// The longest from the reset line to the next loader's banner.
const RESET_AT_MOST: Duration = Duration::from_secs(60);

/// How case 1 or 2 stops the feeder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Signal {
    /// `SIGSTOP`: alive and holding the device, never feeding.
    Stop,
    /// `SIGKILL`: gone, its release without the `V`.
    Kill,
}

/// Cases 1 and 2: the feeder stops feeding, and the report must come. Case
/// 1 also resets by itself and is kept by the next loader.
///
/// Verifies: H.FAIL.14, `L.lockup.6`, `L.lockup.7`, `L.lockup.8`, `L.lockup.9`
fn heartbeat_stops(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
    signal: Signal,
) -> Result<()> {
    let (name, flag) = match signal {
        Signal::Stop => ("stopped", "-STOP"),
        Signal::Kill => ("killed", "-KILL"),
    };
    let reset = signal == Signal::Stop;
    let mut options = format!("{} ferrix.watchdog=user", qemu::init_option(STOP_SCRIPT));
    if reset {
        options.push_str(" ferrix.panic=reset");
    }
    let script = format!(
        "#!/bin/sh\nwatchdog -F -T 5 -t 1 /dev/watchdog &\npid=$!\nsleep 3\necho \"{FEEDING} as $pid\"\nkill {flag} $pid\necho \"{STOPPED}\"\nsleep 600\n"
    );
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        &script,
        &options,
        &["bin/watchdog", "bin/sleep"],
    )?;
    let mut wanted = args.clone();
    wanted.keep_ram = reset;
    println!("  {arch}: the heartbeat's feeder {name}");
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let mut waited = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let stopped = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(STOPPED))
        })?;
        if !stopped {
            return Err(Error::new(format!(
                "{arch}: the script never said `{STOPPED}`"
            )));
        }
        let at = Instant::now();
        let reported = watching.read_more(at + HEART_AT_MOST + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains("FERRIX-PANIC"))
        })?;
        if !reported {
            return Err(Error::new(format!(
                "{arch}: no report within {} s of the feeder {name}",
                (HEART_AT_MOST + PATIENCE).as_secs()
            )));
        }
        waited = Some(at.elapsed());
        let written = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(WRITTEN_BACK))
        })?;
        let _ = written;
        if reset {
            let seen = watching.after().len();
            let came = watching.read_more(Instant::now() + RESET_AT_MOST, |lines| {
                lines
                    .get(seen..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(LOADER_BANNER))
            })?;
            if !came {
                return Err(Error::new(format!(
                    "{arch}: the machine did not reset by itself within {} s of the report",
                    RESET_AT_MOST.as_secs()
                )));
            }
            let banner = watching
                .after()
                .iter()
                .skip(seen)
                .position(|line| line.contains(LOADER_BANNER))
                .map_or(seen, |at| seen + at);
            let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
                lines
                    .get(banner..)
                    .unwrap_or_default()
                    .iter()
                    .any(|line| line.contains(MIRRORING))
            })?;
            boot_1 = watching
                .lines()
                .iter()
                .chain(watching.after().get(..banner).unwrap_or_default())
                .cloned()
                .collect();
            after = watching.after().get(banner..).unwrap_or_default().to_vec();
        } else {
            boot_1 = watching
                .lines()
                .iter()
                .chain(watching.after())
                .cloned()
                .collect();
        }
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(
        arch,
        &image,
        &parts.kernel.elf,
        &wanted,
        qemu::SUCCESS_MARKER,
        hook,
    )?;
    let kept = if reset {
        let disk = read(&image)?;
        Some(lastkernel(&disk))
    } else {
        None
    };
    heartbeat_problem(&boot_1, &after, waited, wanted.smp, kept.as_ref()).map_or(
        Ok(()),
        |problem| {
            Err(Error::new(format!(
                "{arch}: the heartbeat's feeder {name}: {problem}"
            )))
        },
    )?;
    println!("  {arch}: the feeder {name}, and the report came under FX-9008");
    Ok(())
}

/// The ESP's `FERRIX/LASTKERNEL.TXT`, or why it does not read.
fn lastkernel(disk: &[u8]) -> std::result::Result<String, String> {
    let mut volume =
        ferrix_fat::Volume::open(disk).map_err(|why| format!("the ESP does not read: {why:?}"))?;
    let file = volume
        .read_file("FERRIX/LASTKERNEL.TXT", 4 << 20)
        .map_err(|why| format!("FERRIX/LASTKERNEL.TXT does not read: {why:?}"))?;
    Ok(String::from_utf8_lossy(&file).into_owned())
}

/// Why a heartbeat case does not show the report it must, if it does not:
/// the device served, the report under FX-9008 within the bound, a `cpu`
/// line for each processor, and where the machine reset, the next loader's
/// line and the kept file.
fn heartbeat_problem(
    boot_1: &[String],
    after: &[String],
    waited: Option<Duration>,
    processors: u32,
    kept: Option<&std::result::Result<String, String>>,
) -> Option<String> {
    if !boot_1.iter().any(|line| line.contains(SERVED)) {
        return Some(format!("the kernel never said `{}...`", SERVED.trim()));
    }
    let Some(panic) = boot_1.iter().position(|line| line.contains("FERRIX-PANIC")) else {
        return Some("no failure report".to_owned());
    };
    let report = boot_1.get(panic..).unwrap_or_default();
    if !report.iter().any(|line| line.contains(USER_LOCKUP_CODE)) {
        return Some(format!(
            "the report is not the user watchdog's: no `{}` line; it began `{}`",
            USER_LOCKUP_CODE.trim(),
            boot_1.get(panic).map_or("", |line| line.trim())
        ));
    }
    let waited = waited?;
    if waited < HEART_AT_LEAST || waited > HEART_AT_MOST {
        return Some(format!(
            "the report came {} ms after the stop line, outside {} to {} s",
            waited.as_millis(),
            HEART_AT_LEAST.as_secs_f32(),
            HEART_AT_MOST.as_secs()
        ));
    }
    for cpu in 0..processors.max(1) {
        let wanted = format!("{CPU_LINE}{cpu:<4}  ");
        if !report
            .iter()
            .any(|line| line.contains(&wanted) && line.contains(" beats, "))
        {
            return Some(format!(
                "the report has no `cpu` line for processor {cpu}"
            ));
        }
    }
    let Some(kept) = kept else {
        return None;
    };
    let ended = format!("pstore   boot 1 ended: {}", Stop::Lockup.says());
    if !after.iter().any(|line| line.trim_end().ends_with(&ended)) {
        return Some(format!("the next loader did not say `{ended}`"));
    }
    match kept {
        Ok(text) if text.contains("FX-9008") => None,
        Ok(_) => Some("FERRIX/LASTKERNEL.TXT holds no FX-9008 line".to_owned()),
        Err(why) => Some(why.clone()),
    }
}

/// Case 3: the feeder goes on, a second open is refused, a magic close
/// disarms, a timeout of 0 is refused, and nothing is reported.
///
/// Verifies: `L.lockup.6`, `L.lockup.7`
fn heartbeat_goes_on(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.watchdog=user", qemu::init_option(STOP_SCRIPT));
    let script = format!(
        "#!/bin/sh\n\
         watchdog -F -T 5 -t 1 /dev/watchdog &\n\
         pid=$!\n\
         sleep 1\n\
         echo \"{FEEDING} as $pid\"\n\
         watchdog -F -T 5 /dev/watchdog\n\
         echo \"{SECOND_OPEN}\"\n\
         sleep 14\n\
         kill -TERM $pid\n\
         wait $pid\n\
         echo \"{MAGIC_SENT}\"\n\
         watchdog -F -T 0 -t 1 /dev/watchdog &\n\
         zero=$!\n\
         sleep 1\n\
         kill -TERM $zero\n\
         wait $zero\n\
         sleep 10\n\
         echo \"{STILL}\"\n\
         poweroff -f\n"
    );
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        &script,
        &options,
        &["bin/watchdog", "bin/sleep", "bin/poweroff"],
    )?;
    println!("  {arch}: the heartbeat goes on for 15 s, then a magic close");
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines
                .iter()
                .any(|line| line.contains(STILL) || line.contains("FERRIX-PANIC"))
        })?;
        lines = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(
        arch,
        &image,
        &parts.kernel.elf,
        args,
        qemu::SUCCESS_MARKER,
        hook,
    )?;
    goes_on_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!(
            "{arch}: the heartbeat that goes on: {problem}"
        )))
    })?;
    println!("  {arch}: fed, refused a second open and a timeout of 0, closed with the magic `V`, and no report");
    Ok(())
}

/// Why case 3 does not show what it must, if it does not.
fn goes_on_problem(lines: &[String]) -> Option<String> {
    if let Some(report) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Some(format!("a report came while fed: `{}`", report.trim()));
    }
    let order = [FEEDING, BUSY, SECOND_OPEN, MAGIC_DISARMED, MAGIC_SENT, INVALID, STILL];
    let mut from = 0;
    for wanted in order {
        let Some(at) = lines
            .iter()
            .skip(from)
            .position(|line| line.contains(wanted))
        else {
            return Some(format!("no `{wanted}` line after the ones before it"));
        };
        from += at;
    }
    None
}

/// Case 4: init feeds under its own option, and nothing is reported.
///
/// Verifies: `L.lockup.7`
fn init_feeds(arch: Arch, args: &Args) -> Result<()> {
    let mut files = crate::init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = read(&crate::busybox::program(arch)?)?;
    files.push(crate::ports::File {
        path: "bin/sh".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(busybox),
    });
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel(arch, args.release)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &files)?;
    let options = format!(
        "{} ferrix.watchdog=user ferrix.init.watchdog=5",
        crate::init::command_line().trim_end()
    );
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!("  {arch}: init feeds /dev/watchdog for {INIT_SECONDS} s");
    let mut lines = Vec::new();
    let mut fed_for = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let feeds = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(INIT_FEEDS))
        })?;
        if feeds {
            let at = Instant::now();
            let _ = watching.read_more(at + Duration::from_secs(INIT_SECONDS), |lines| {
                lines.iter().any(|line| line.contains("FERRIX-PANIC"))
            })?;
            fed_for = Some(at.elapsed());
        }
        lines = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &image, &kernel, args, qemu::SUCCESS_MARKER, hook)?;
    if !lines.iter().any(|line| line.contains(INIT_FEEDS)) {
        return Err(Error::new(format!(
            "{arch}: init never said `{INIT_FEEDS}...`"
        )));
    }
    if let Some(report) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Err(Error::new(format!(
            "{arch}: a report came while init fed: `{}`",
            report.trim()
        )));
    }
    if fed_for.is_none_or(|fed| fed < Duration::from_secs(INIT_SECONDS)) {
        return Err(Error::new(format!(
            "{arch}: the boot ended before {INIT_SECONDS} s of feeding"
        )));
    }
    println!("  {arch}: init fed /dev/watchdog for {INIT_SECONDS} s, and no report");
    Ok(())
}

/// Init's line once it feeds.
const INIT_FEEDS: &str = "init     feeding /dev/watchdog every ";
/// How long case 4 watches init feed: four of its timeouts.
const INIT_SECONDS: u64 = 20;

/// Case 5: without `ferrix.watchdog`, no node.
///
/// Verifies: `L.lockup.6`
fn heartbeat_off(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let script = "#!/bin/sh\nif [ -e /dev/watchdog ]; then echo \"lk-off: present\"; else echo \"lk-off: absent\"; fi\npoweroff -f\n";
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        script,
        &qemu::init_option(STOP_SCRIPT),
        &["bin/poweroff"],
    )?;
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains("lk-off: "))
        })?;
        lines = watching.after().to_vec();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(
        arch,
        &image,
        &parts.kernel.elf,
        args,
        qemu::SUCCESS_MARKER,
        hook,
    )?;
    if !lines.iter().any(|line| line.contains("lk-off: absent")) {
        return Err(Error::new(format!(
            "{arch}: without ferrix.watchdog, /dev/watchdog was not absent"
        )));
    }
    println!("  {arch}: without ferrix.watchdog, no /dev/watchdog");
    Ok(())
}

/// The check's line as the masked processor begins to spin.
const MASKING: &str = "ferrix.check.lockup=irqoff: processor ";
/// The detector's line once it watches.
const WATCHING: &str = "  lockup   ferrix.lockup=on: ";
/// The catalog's code for a lockup.
const LOCKUP_CODE: &str = "  code      FX-9007";
/// The earliest and latest the report may come after the masking line:
/// the detector's ten seconds less the slack of reading the port, and those
/// plus its tick, the NMI's two seconds and a loaded host's delay.
const LOCKUP_AT_LEAST: Duration = Duration::from_millis(9_500);
const LOCKUP_AT_MOST: Duration = Duration::from_secs(30);

/// The lockup: with `ferrix.lockup=on`, the check masks the boot
/// processor's interrupts and spins; its watcher must report it within the
/// bound, and after a reset the next loader must say the boot ended in a
/// lockup.
///
/// Verifies: H.FAIL.6, `L.lockup.1`, `L.lockup.2`, `L.lockup.3`, `L.console.49`
fn locked_up(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut wanted = args.clone();
    wanted.kernel_options.push("ferrix.lockup=on".to_owned());
    wanted
        .kernel_options
        .push("ferrix.check.lockup=irqoff".to_owned());
    wanted.keep_ram = true;
    let port = crate::display::free_port()?;
    wanted.qmp_port = Some(port);
    let (image, kernel) = build(arch, &wanted)?;
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let mut waited = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let masked = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(MASKING))
        })?;
        if !masked {
            return Err(Error::new(format!(
                "{arch}: the check never said `{MASKING}...`"
            )));
        }
        let at = Instant::now();
        let reported = watching.read_more(at + LOCKUP_AT_MOST + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(WRITTEN_BACK))
        })?;
        if !reported {
            return Err(Error::new(format!(
                "{arch}: no lockup report within {} s of the masking line",
                (LOCKUP_AT_MOST + PATIENCE).as_secs()
            )));
        }
        waited = watching
            .after()
            .iter()
            .position(|line| line.contains("FERRIX-PANIC"))
            .map(|_| at.elapsed());
        boot_1 = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        let mut qmp = Qmp::connect(port, Instant::now() + PATIENCE)?;
        let _ = qmp.execute("system_reset", None)?;
        let seen = watching.after().len();
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines
                .get(seen..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(MIRRORING))
        })?;
        after = watching.after().get(seen..).unwrap_or_default().to_vec();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &image, &kernel, &wanted, qemu::SUCCESS_MARKER, hook)?;
    // The report's addresses named from the image booted, as the boot test
    // names them, so the judge can tell whose report it is: the trace, and
    // the instruction the NMI interrupted, named once the slide is known
    // (the report says it after the registers).
    if let Some(names) = crate::symbolize::Symbolizer::open(&kernel) {
        boot_1 = boot_1
            .iter()
            .map(|line| names.annotate(line).unwrap_or_else(|| line.clone()))
            .collect();
        boot_1 = boot_1
            .iter()
            .map(|line| interrupted_named(&names, line).unwrap_or_else(|| line.clone()))
            .collect();
    }
    lockup_problem(arch, &boot_1, &after, waited).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: the lockup: {problem}")))
    })?;
    println!(
        "  {arch}: the lockup was reported, and the next loader said the boot ended in a lockup"
    );
    Ok(())
}

/// The register dump's `rip` line with the function it is in, named as a
/// trace line is: `None` for any other line, or one that names nothing.
fn interrupted_named(names: &crate::symbolize::Symbolizer, line: &str) -> Option<String> {
    let (_, rest) = line.split_once(RIP)?;
    let address = rest.split_whitespace().next()?;
    let named = names.annotate(&format!("  trace     #0  {address}"))?;
    let function = named.split_once(address)?.1.trim();
    Some(format!("{line}  ({function})"))
}

/// What the register dump of an x86-64 trap report calls the interrupted
/// instruction.
const RIP: &str = "  rip      ";

/// Why the lockup case does not show a lockup caught and reported, if it
/// does not: the detector watching, the check masking a processor, the
/// report with the catalog's code within the bound, on x86-64 from the
/// masked processor itself and through the check's spin, and the next
/// loader's line.
fn lockup_problem(
    arch: Arch,
    boot_1: &[String],
    after: &[String],
    waited: Option<Duration>,
) -> Option<String> {
    if !boot_1.iter().any(|line| line.contains(WATCHING)) {
        return Some(format!("the detector never said `{}...`", WATCHING.trim()));
    }
    let Some(masked) = boot_1
        .iter()
        .find_map(|line| line.split_once(MASKING).map(|(_, rest)| rest))
        .and_then(|rest| rest.split_whitespace().next())
    else {
        return Some(format!("the check never said `{MASKING}...`"));
    };
    let Some(panic) = boot_1.iter().position(|line| line.contains("FERRIX-PANIC")) else {
        return Some("no failure report after the masking line".to_owned());
    };
    let report = boot_1.get(panic..).unwrap_or_default();
    if !report.iter().any(|line| line.contains(LOCKUP_CODE)) {
        return Some(format!(
            "the report is not the lockup's: no `{}` line; it began `{}`",
            LOCKUP_CODE.trim(),
            boot_1.get(panic).map_or("", |line| line.trim())
        ));
    }
    let waited = waited?;
    if waited < LOCKUP_AT_LEAST || waited > LOCKUP_AT_MOST {
        return Some(format!(
            "the report came {} ms after the masking line, outside {} to {} s",
            waited.as_millis(),
            LOCKUP_AT_LEAST.as_secs_f32(),
            LOCKUP_AT_MOST.as_secs()
        ));
    }
    if arch == Arch::X86_64 {
        let own = format!("  on        processor {masked} ");
        if !report.iter().any(|line| line.contains(&own)) {
            return Some(format!(
                "the report is not the masked processor's own: no `{}` line",
                own.trim()
            ));
        }
        let Some(interrupted) = report.iter().find(|line| line.contains(RIP)) else {
            return Some("the report has no `rip` line".to_owned());
        };
        if !interrupted.contains("lockup_check") {
            return Some(format!(
                "the NMI did not interrupt the check's spin: `{}`",
                interrupted.trim()
            ));
        }
    } else {
        let named = format!("processor {masked} took no interrupt for ");
        if !report.iter().any(|line| line.contains(&named)) {
            return Some(format!("the watcher's report does not say `{named}...`"));
        }
    }
    let ended = format!("pstore   boot 1 ended: {}", Stop::Lockup.says());
    if !after.iter().any(|line| line.trim_end().ends_with(&ended)) {
        return Some(format!("the next loader did not say `{ended}`"));
    }
    None
}

/// What the quiet case's script says, before and after its sleep.
const QUIET_START: &str = "pm-watch: idle with the detector on";
const QUIET_END: &str = "pm-watch: still running";
/// How long the quiet case's script sleeps: past two of the detector's
/// bounds, with every processor idle.
const QUIET_SECONDS: u64 = 25;

/// The false-positive case: `ferrix.lockup=on`, and a script that sleeps
/// for [`QUIET_SECONDS`] with every processor idle, then powers off. The
/// detector must watch the whole time and report nothing: idle processors
/// tick and beat (`L.lockup.2`).
///
/// Verifies: `L.lockup.2`
fn watched_quietly(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.lockup=on", qemu::init_option(STOP_SCRIPT));
    let script = format!(
        "#!/bin/sh\necho \"{QUIET_START}\"\nsleep {QUIET_SECONDS}\necho \"{QUIET_END}\"\npoweroff -f\n"
    );
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        &script,
        &options,
        &["bin/poweroff", "bin/sleep"],
    )?;
    println!("  {arch}: {QUIET_SECONDS} s idle with ferrix.lockup=on");
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines
                .iter()
                .any(|line| line.contains(QUIET_END) || line.contains("FERRIX-PANIC"))
        })?;
        lines = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(
        arch,
        &image,
        &parts.kernel.elf,
        args,
        qemu::SUCCESS_MARKER,
        hook,
    )?;
    quiet_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: the quiet case: {problem}")))
    })?;
    println!("  {arch}: {QUIET_SECONDS} s idle with the detector on, and no report");
    Ok(())
}

/// Why the quiet case shows no quiet watch, if it does not: the detector
/// watched, the script slept and woke, and nothing was reported.
fn quiet_problem(lines: &[String]) -> Option<String> {
    if !lines.iter().any(|line| line.contains(WATCHING)) {
        return Some(format!("the detector never said `{}...`", WATCHING.trim()));
    }
    if let Some(report) = lines.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Some(format!(
            "a report came with nothing stuck: `{}`",
            report.trim()
        ));
    }
    if !lines.iter().any(|line| line.contains(QUIET_START)) {
        return Some(format!("the script never said `{QUIET_START}`"));
    }
    if !lines.iter().any(|line| line.contains(QUIET_END)) {
        return Some(format!("the script never said `{QUIET_END}`"));
    }
    None
}

