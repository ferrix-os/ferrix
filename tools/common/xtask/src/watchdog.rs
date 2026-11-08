//! `cargo xtask test-watchdog`: a feed that stops ends in a report and a
//! reset, by software or by the chipset's TCO timer (`docs/POSTMORTEM.md`
//! §6.11). x86-64 on q35, whose ICH9 has the TCO.
//!
//! Every case boots busybox as init with `ferrix.watchdog=on`, except the
//! last two:
//!
//! 1. *stopped, reset*: busybox's `watchdog -F -T 5 -t 1` feeds
//!    `/dev/watchdog`, then is stopped with `SIGSTOP`. The report must come
//!    under FX-9008 3.5 to 15 s later, say the TCO covers it, and under
//!    `ferrix.panic=reset` the machine reset itself; boot 2's loader must
//!    quote the report, and boot 2's kernel not say the TCO reset it (the
//!    report's own reset came first and halted the timer);
//! 2. *stopped, halt*: the same with the default `ferrix.panic=halt`: the
//!    report halts every processor with interrupts masked, the strongest
//!    "stuck" there is, and the TCO alone must reset the machine 15 to 45 s
//!    after the report; boot 2's kernel must say the TCO reset it, and boot
//!    2's loader quote the report, so the record outlived a hardware reset;
//! 3. *goes on*: the feeder fed for 15 s, a second open refused `EBUSY`, then
//!    `SIGTERM` (busybox writes `V` and closes: the magic close), a timeout of
//!    0 refused `EINVAL`, and 40 s more -- past the TCO's 35 s -- with no
//!    report and no reset;
//! 4. *init feeds*: init as pid 1 with `ferrix.init.watchdog=5` for 45 s:
//!    init says it feeds, and no report, no reset;
//! 5. *off*: without `ferrix.watchdog`, `/dev/watchdog` is absent.
//!
//! `FERRIX_TEST_WATCHDOG_ONLY=<case>,...` (`reset`, `halt`, `goes-on`,
//! `init`, `off`) runs those cases alone, for a negative control's run; a
//! gate row sets nothing and runs every case.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::postmortem::{LOADER_BANNER, MIRRORING, PATIENCE, STOP_SCRIPT, read};
use crate::qemu::{self, Watching};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-watchdog",
    help_at: 502,
    help: &[
        "test-watchdog  /dev/watchdog (ferrix.watchdog=on): a feeder that stops is reported and the machine reset, by ferrix.panic=reset or by the TCO alone; a feed that goes on, init's feed and the option off (x86_64)",
    ],
    run: |_, args| test_watchdog(args),
}];

/// `cargo xtask test-watchdog`, for each architecture asked.
pub(crate) fn test_watchdog(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        if arch != Arch::X86_64 {
            return Err(Error::new(
                "test-watchdog runs on x86_64: the TCO timer it resets with is q35's ICH9's",
            ));
        }
        let only = std::env::var("FERRIX_TEST_WATCHDOG_ONLY").unwrap_or_default();
        let wants = |case: &str| only.is_empty() || only.split(',').any(|each| each == case);
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
        if wants("reset") {
            feeder_stops(arch, args, &parts, &shell, Panic::Reset)?;
        }
        if wants("halt") {
            feeder_stops(arch, args, &parts, &shell, Panic::Halt)?;
        }
        if wants("goes-on") {
            feed_goes_on(arch, args, &parts, &shell)?;
        }
        if wants("init") {
            init_feeds(arch, args)?;
        }
        if wants("off") {
            switched_off(arch, args, &parts, &shell)?;
        }
        println!("  {arch}: test-watchdog passed");
    }
    Ok(())
}

/// The report's code for user space that stopped feeding its watchdog.
const STARVED_CODE: &str = "  code      FX-9008";
/// The report's line that the TCO stands behind it.
const BACKSTOP: &str = "  watchdog  the TCO resets the machine within ";
/// The kernel's lines once the watchdog is served, and the TCO taken.
const SERVED: &str = "  watchdog ferrix.watchdog=on: /dev/watchdog is served";
const TCO_TAKEN: &str = "  watchdog TCO version ";
/// Boot 2's kernel's line after a TCO reset.
const TCO_RESET_IT: &str = "  watchdog the TCO reset the previous boot";
/// Boot 2's loader's quotation of boot 1's report.
const QUOTED: &str = "'s last report: panicked: user space stopped feeding /dev/watchdog";
/// What the scripts say.
const FEEDING: &str = "wd-feed: feeding";
const STOPPED: &str = "wd-feed: the feeder is stopped";
const SECOND_OPEN: &str = "wd-feed: second open done";
const MAGIC_SENT: &str = "wd-feed: magic close sent";
const STILL: &str = "wd-feed: still running";
/// busybox's words for the refusals.
const BUSY: &str = "Resource busy";
const INVALID: &str = "Invalid argument";
/// The kernel's line for the magic close.
const MAGIC_DISARMED: &str = "closed it with the magic `V`: disarmed";
/// The bounds of the report after the stop line: the timeout less the feed
/// period and the port's slack, and the timeout plus the task's second and
/// a loaded host's delay.
const REPORT_AT_LEAST: Duration = Duration::from_millis(3_500);
const REPORT_AT_MOST: Duration = Duration::from_secs(15);
/// The bounds of the TCO's reset after the report: the panic count, 25.2 s,
/// less the slack of reading the port, and plus a loaded host's.
const TCO_AT_LEAST: Duration = Duration::from_secs(15);
const TCO_AT_MOST: Duration = Duration::from_secs(45);
/// The longest from the report to `ferrix.panic=reset`'s reset.
const RESET_AT_MOST: Duration = Duration::from_secs(60);

/// What boot 1's report does after its last line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Panic {
    /// `ferrix.panic=reset`: the report's own reset.
    Reset,
    /// The default halt: the TCO's reset alone.
    Halt,
}

/// Cases 1 and 2: the feeder stops feeding; the report must come, and the
/// machine reset by itself.
fn feeder_stops(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
    panic: Panic,
) -> Result<()> {
    let mut options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
    if panic == Panic::Reset {
        options.push_str(" ferrix.panic=reset");
    }
    let script = format!(
        "#!/bin/sh\nwatchdog -F -T 5 -t 1 /dev/watchdog &\npid=$!\nsleep 3\necho \"{FEEDING} as $pid\"\nkill -STOP $pid\necho \"{STOPPED}\"\nsleep 600\n"
    );
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        &script,
        &options,
        &["bin/watchdog", "bin/sleep"],
    )?;
    let mut wanted = args.clone();
    wanted.keep_ram = true;
    println!("  {arch}: the feeder stops, under ferrix.panic={panic:?}");
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let mut report_after = None;
    let mut reset_after = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let stopped = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(STOPPED))
        })?;
        if !stopped {
            return Err(Error::new(format!(
                "{arch}: the script never said `{STOPPED}`"
            )));
        }
        let at = Instant::now();
        let reported = watching.read_more(at + REPORT_AT_MOST + PATIENCE, |lines| {
            lines.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        if !reported {
            return Err(Error::new(format!(
                "{arch}: no report within {} s of the feeder's stop",
                (REPORT_AT_MOST + PATIENCE).as_secs()
            )));
        }
        report_after = Some(at.elapsed());
        let reported_at = Instant::now();
        let seen = watching.after().len();
        let came = watching.read_more(reported_at + RESET_AT_MOST + PATIENCE, |lines| {
            lines
                .get(seen..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(LOADER_BANNER))
        })?;
        if !came {
            return Err(Error::new(format!(
                "{arch}: the machine did not reset by itself within {} s of the report",
                (RESET_AT_MOST + PATIENCE).as_secs()
            )));
        }
        reset_after = Some(reported_at.elapsed());
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
                .any(|line| line.contains(SERVED))
        })?;
        boot_1 = watching
            .lines()
            .iter()
            .chain(watching.after().get(..banner).unwrap_or_default())
            .cloned()
            .collect();
        after = watching.after().get(banner..).unwrap_or_default().to_vec();
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
    stopped_problem(&boot_1, &after, report_after, reset_after, panic).map_or(
        Ok(()),
        |problem| {
            Err(Error::new(format!(
                "{arch}: the feeder stopped under ferrix.panic={panic:?}: {problem}"
            )))
        },
    )?;
    println!(
        "  {arch}: the feeder stopped: FX-9008 {} ms later, and {} reset the machine {} ms after it; boot 2's loader quoted the report",
        report_after.map_or(0, |taken| taken.as_millis()),
        match panic {
            Panic::Reset => "ferrix.panic=reset",
            Panic::Halt => "the TCO alone",
        },
        reset_after.map_or(0, |taken| taken.as_millis()),
    );
    Ok(())
}

/// Why cases 1 and 2 do not show what they must, if they do not.
fn stopped_problem(
    boot_1: &[String],
    after: &[String],
    report_after: Option<Duration>,
    reset_after: Option<Duration>,
    panic: Panic,
) -> Option<String> {
    for wanted in [SERVED, TCO_TAKEN] {
        if !boot_1.iter().any(|line| line.contains(wanted)) {
            return Some(format!("boot 1's kernel never said `{}...`", wanted.trim()));
        }
    }
    let Some(start) = boot_1
        .iter()
        .position(|line| line.contains(qemu::PANIC_MARKER))
    else {
        return Some("no failure report".to_owned());
    };
    let report = boot_1.get(start..).unwrap_or_default();
    if !report.iter().any(|line| line.contains(STARVED_CODE)) {
        return Some(format!(
            "the report is not the watchdog's: no `{}` line; it began `{}`",
            STARVED_CODE.trim(),
            boot_1.get(start).map_or("", |line| line.trim())
        ));
    }
    if !report.iter().any(|line| line.contains(BACKSTOP)) {
        return Some(format!("the report has no `{}...` line", BACKSTOP.trim()));
    }
    let report_after = report_after?;
    if report_after < REPORT_AT_LEAST || report_after > REPORT_AT_MOST {
        return Some(format!(
            "the report came {} ms after the stop line, outside {} to {} s",
            report_after.as_millis(),
            REPORT_AT_LEAST.as_secs_f32(),
            REPORT_AT_MOST.as_secs()
        ));
    }
    let reset_after = reset_after?;
    let by_tco = after.iter().any(|line| line.contains(TCO_RESET_IT));
    match panic {
        Panic::Reset if by_tco => {
            return Some(format!(
                "boot 2's kernel said `{}`, but ferrix.panic=reset's own reset should have come first",
                TCO_RESET_IT.trim()
            ));
        }
        Panic::Reset => {}
        Panic::Halt => {
            if !by_tco {
                return Some(format!(
                    "boot 2's kernel never said `{}...`",
                    TCO_RESET_IT.trim()
                ));
            }
            if reset_after < TCO_AT_LEAST || reset_after > TCO_AT_MOST {
                return Some(format!(
                    "the TCO reset the machine {} ms after the report, outside {} to {} s",
                    reset_after.as_millis(),
                    TCO_AT_LEAST.as_secs(),
                    TCO_AT_MOST.as_secs()
                ));
            }
        }
    }
    if !after.iter().any(|line| line.contains(QUOTED)) {
        return Some(format!("boot 2's loader never said `boot 1{QUOTED}...`"));
    }
    if !after.iter().any(|line| line.contains(MIRRORING)) {
        return Some(format!("boot 2's kernel never said `{}`", MIRRORING.trim()));
    }
    None
}

/// Case 3: the feeder goes on, a second open is refused, a magic close
/// disarms, a timeout of 0 is refused, and nothing is reported or reset.
fn feed_goes_on(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
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
         sleep 40\n\
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
    println!("  {arch}: the feed goes on for 15 s, then a magic close and 40 s more");
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| {
                line.contains(STILL)
                    || line.contains(qemu::PANIC_MARKER)
                    || line.contains(LOADER_BANNER)
            })
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
            "{arch}: the feed that goes on: {problem}"
        )))
    })?;
    println!(
        "  {arch}: fed, refused a second open and a timeout of 0, closed with the magic `V`, and no report or reset"
    );
    Ok(())
}

/// Why case 3 does not show what it must, if it does not.
fn goes_on_problem(lines: &[String]) -> Option<String> {
    if let Some(report) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Some(format!("a report came while fed: `{}`", report.trim()));
    }
    let first_banner = lines.iter().position(|line| line.contains(LOADER_BANNER));
    if let Some(first) = first_banner
        && lines
            .iter()
            .skip(first + 1)
            .any(|line| line.contains(LOADER_BANNER))
    {
        return Some("the machine reset while fed or after the magic close".to_owned());
    }
    let order = [
        FEEDING,
        BUSY,
        SECOND_OPEN,
        MAGIC_DISARMED,
        MAGIC_SENT,
        INVALID,
        STILL,
    ];
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

/// Init's line once it feeds.
const INIT_FEEDS: &str = "feeding /dev/watchdog every ";
/// How long case 4 watches init feed: past the TCO's count of 35 s.
const INIT_SECONDS: u64 = 45;

/// Case 4: init feeds under its own option, and nothing is reported or
/// reset.
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
        "{} ferrix.watchdog=on ferrix.init.watchdog=5",
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
                lines
                    .iter()
                    .any(|line| line.contains(qemu::PANIC_MARKER) || line.contains(LOADER_BANNER))
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
    if let Some(report) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Err(Error::new(format!(
            "{arch}: a report came while init fed: `{}`",
            report.trim()
        )));
    }
    if lines
        .iter()
        .filter(|line| line.contains(LOADER_BANNER))
        .count()
        > 1
    {
        return Err(Error::new(format!(
            "{arch}: the machine reset while init fed"
        )));
    }
    if fed_for.is_none_or(|fed| fed < Duration::from_secs(INIT_SECONDS)) {
        return Err(Error::new(format!(
            "{arch}: the boot ended before {INIT_SECONDS} s of feeding"
        )));
    }
    println!("  {arch}: init fed /dev/watchdog for {INIT_SECONDS} s, and no report or reset");
    Ok(())
}

/// Case 5: without `ferrix.watchdog`, no node.
fn switched_off(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let script = "#!/bin/sh\nif [ -e /dev/watchdog ]; then echo \"wd-off: present\"; else echo \"wd-off: absent\"; fi\npoweroff -f\n";
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
            lines.iter().any(|line| line.contains("wd-off: "))
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
    if !lines.iter().any(|line| line.contains("wd-off: absent")) {
        return Err(Error::new(format!(
            "{arch}: without ferrix.watchdog, /dev/watchdog was not absent"
        )));
    }
    if lines.iter().any(|line| line.contains(TCO_TAKEN)) {
        return Err(Error::new(format!(
            "{arch}: without ferrix.watchdog, the kernel took the TCO"
        )));
    }
    println!("  {arch}: without ferrix.watchdog, no /dev/watchdog and the TCO left alone");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_halted_report_needs_the_tco_to_say_it_reset() {
        let boot_1 = lines(&[
            "  watchdog TCO version 2 at I/O 0x660, halted until opened",
            "  watchdog ferrix.watchdog=on: /dev/watchdog is served; once opened",
            "FERRIX-PANIC user space stopped feeding /dev/watchdog: armed by process 7",
            "  watchdog  the TCO resets the machine within 25 s, whatever this report does",
            "  code      FX-9008  user space stopped feeding its watchdog",
        ]);
        let quoted = "  pstore   boot 1's last report: panicked: user space stopped feeding /dev/watchdog: armed";
        let after = lines(&[quoted, "  pstore   mirroring the log to 0x1000000"]);
        let report = Some(Duration::from_secs(6));
        let reset = Some(Duration::from_secs(26));
        let problem = stopped_problem(&boot_1, &after, report, reset, Panic::Halt);
        assert!(problem.is_some_and(|why| why.contains("the TCO reset the previous boot")));
        let after = lines(&[
            "  watchdog the TCO reset the previous boot: its feed stopped (SECOND_TO_STS, cleared)",
            quoted,
            "  pstore   mirroring the log to 0x1000000",
        ]);
        assert_eq!(
            stopped_problem(&boot_1, &after, report, reset, Panic::Halt),
            None
        );
        assert!(
            stopped_problem(&boot_1, &after, report, reset, Panic::Reset)
                .is_some_and(|why| why.contains("should have come first"))
        );
        let late = Some(Duration::from_secs(50));
        assert!(
            stopped_problem(&boot_1, &after, report, late, Panic::Halt)
                .is_some_and(|why| why.contains("outside 15 to 45 s"))
        );
    }

    #[test]
    fn a_report_while_fed_or_a_second_banner_fails_the_case_that_goes_on() {
        let good = lines(&[
            "Ferrix loader 0.3.0",
            "wd-feed: feeding as 9",
            "watchdog: /dev/watchdog: Resource busy",
            "wd-feed: second open done",
            "  watchdog process 9 closed it with the magic `V`: disarmed",
            "wd-feed: magic close sent",
            "watchdog: WDIOC_SETTIMEOUT: Invalid argument",
            "wd-feed: still running",
        ]);
        assert_eq!(goes_on_problem(&good), None);
        let mut reset = good.clone();
        reset.insert(6, "Ferrix loader 0.3.0".to_owned());
        assert!(goes_on_problem(&reset).is_some_and(|why| why.contains("reset")));
        let mut reported = good;
        reported.push("FERRIX-PANIC user space stopped feeding".to_owned());
        assert!(goes_on_problem(&reported).is_some_and(|why| why.contains("report")));
    }
}
