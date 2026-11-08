//! `cargo xtask test-watchdog`: a feed that stops ends in a report, and a
//! feed that goes on, a magic close, `WDIOS_DISABLECARD` and a stop on
//! purpose end in none (`docs/POSTMORTEM.md` §6.11, `H.FAIL.14`,
//! `L.lockup.6`-`9`). x86-64: the watchdog has no architecture's code, and
//! its windows are measured where the gate host boots fastest.
//!
//! Every case boots busybox as init with `ferrix.watchdog=on`, except the
//! last two. busybox's `watchdog -F -T 5 -t 1` arms a 5 s timeout and feeds
//! every second.
//!
//! 1. *stopped*: the feeder is stopped with `SIGSTOP`. The report must come
//!    under FX-9008 within the window (`REPORT_AT_LEAST` to
//!    `REPORT_AT_MOST` after the stop line), name the opener, its 5 s
//!    timeout and the time since the last feed, and halt;
//! 2. *stopped, reset*: the same under `ferrix.panic=reset`: the report's
//!    reset line, the machine starting again by itself, and boot 2's loader
//!    quoting the report;
//! 3. *killed*: the feeder is ended with `SIGKILL`, so its file's last
//!    release comes without the magic `V`: the kernel says it stays armed,
//!    and FX-9008 comes within the window;
//! 4. *goes on*: the feeder feeds for 20 s, four timeouts; a second open is
//!    refused `EBUSY`; `SIGTERM` makes busybox write `V` and close, the
//!    magic close, which disarms; a timeout of 0 is refused `EINVAL`; and
//!    10 s more, the timeout and 5 s, with no report;
//! 5. *disabled*: `watchdog-test` (`src/tests/watchdog`) opens it, sets a 5 s
//!    timeout, disarms it with `WDIOS_DISABLECARD`, writes to it and holds it open: no report in
//!    the timeout and 5 s, since a feed never arms a disarmed deadline;
//! 6. *halted*: the feeder runs, then `halt -f` (`reboot(2)`'s halt): the
//!    kernel says the watchdog is disarmed for the halt, and no FX-9008. The
//!    halt stops the machine at once, so the task never outlives it: the
//!    line is the check;
//! 7. *init feeds*: init as pid 1 with `ferrix.init.watchdog=5` for 20 s:
//!    init says it feeds, and no report;
//! 8. *off*: `ferrix.watchdog=On`, any value but `on`, is said and taken as
//!    off: `/dev/watchdog` is absent and nothing is served.
//!
//! 9. to 12., init as pid 1 (`crate::watchdog_init`, `docs/WATCHDOG-STAGES.md`
//!    §8): *service*, a service that stops answering its watchdog is ended
//!    and started again; *blocked*, init's loop blocked 20 s is said and
//!    still fed; *stall*, blocked past `ferrix.init.watchdog.stall=` it is no
//!    longer fed and FX-9008 follows; *user*, hung user programs end in no
//!    report, and pid 1 stopped ends in FX-9008.
//!
//! On AArch64 and ARMv7-A only case 7 runs: init's feeder is a thread, new
//! code on every architecture.
//!
//! `FERRIX_TEST_WATCHDOG_ONLY=<case>,...` (`stopped`, `reset`, `killed`,
//! `goes-on`, `disabled`, `halted`, `init`, `off`, `service`, `blocked`,
//! `stall`, `user`, `lode`, `auth`) runs those cases alone,
//! for a negative control's run; a gate row sets nothing and runs every case.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::postmortem::{LOADER_BANNER, PATIENCE, RESET_LINE, STOP_SCRIPT, read};
use crate::qemu::{self, Watching};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-watchdog",
    help_at: 502,
    help: &[
        "test-watchdog  /dev/watchdog (ferrix.watchdog=on): a feeder stopped or killed is reported under FX-9008, and reset by ferrix.panic=reset; a feed that goes on, a magic close, DISABLECARD, a halt, init's feed and the option off are not; init ends a service that misses its watchdog, its own blocked loop is still fed until its stall bound, and hung user programs reset nothing (x86_64; case 7 on every architecture)",
    ],
    run: |_, args| test_watchdog(args),
}];

/// The guest program of case 5, `src/tests/watchdog`.
const TEST_PROGRAM: &str = "watchdog";
/// Where case 5's program goes.
const TEST_PATH: &str = "bin/watchdog-test";

/// `cargo xtask test-watchdog`, for each architecture asked.
pub(crate) fn test_watchdog(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        let only = std::env::var("FERRIX_TEST_WATCHDOG_ONLY").unwrap_or_default();
        let wants = |case: &str| only.is_empty() || only.split(',').any(|each| each == case);
        if arch != Arch::X86_64 {
            // Init's feeder is a thread of pid 1, new code on every
            // architecture: case 7 runs on each (the consultant's E3). The
            // rest are x86-64's, where the windows are measured.
            if !only.is_empty() && !wants("init") {
                return Err(Error::new(
                    "test-watchdog's cases other than `init` run on x86_64: the watchdog has no \
                     architecture's code, and its windows are measured under KVM",
                ));
            }
            init_feeds(arch, args)?;
            println!("  {arch}: test-watchdog passed (case 7, init feeds)");
            continue;
        }
        let busybox = crate::busybox::program(arch)?;
        let shell = read(&busybox)?;
        let loader = crate::cargo::build_loader(arch, args.release)?;
        let kernel = crate::cargo::build_kernel_with_init(arch, args.release, &busybox, "")?;
        let natives = crate::native::build(arch, args.release)?;
        let mut carried = crate::shell::carried_for(arch, &busybox, args)?;
        if wants("disabled") {
            let program = crate::sem::build_test(TEST_PROGRAM, arch, None, false)?;
            carried.push(crate::ports::File {
                path: TEST_PATH.to_owned(),
                mode: 0o755,
                content: crate::ports::Content::Bytes(read(&program)?),
            });
        }
        let parts = crate::init_file::Parts {
            arch,
            loader: &loader,
            kernel: &kernel,
            natives: &natives,
            program: &busybox,
            carried: &carried,
        };
        if wants("stopped") {
            feeder_stops(arch, args, &parts, &shell, Ending::Stopped)?;
        }
        if wants("reset") {
            feeder_stops(arch, args, &parts, &shell, Ending::StoppedReset)?;
        }
        if wants("killed") {
            feeder_stops(arch, args, &parts, &shell, Ending::Killed)?;
        }
        if wants("goes-on") {
            feed_goes_on(arch, args, &parts, &shell)?;
        }
        if wants("disabled") {
            disabled(arch, args, &parts, &shell)?;
        }
        if wants("halted") {
            halted(arch, args, &parts, &shell)?;
        }
        if wants("init") {
            init_feeds(arch, args)?;
        }
        if wants("off") {
            switched_off(arch, args, &parts, &shell)?;
        }
        if wants("service") {
            crate::watchdog_init::service(arch, args)?;
        }
        if wants("blocked") {
            crate::watchdog_init::blocked(arch, args)?;
        }
        if wants("stall") {
            crate::watchdog_init::stall(arch, args)?;
        }
        if wants("user") {
            crate::watchdog_init::user(arch, args)?;
        }
        if wants("lode") {
            crate::watchdog_init::lode(arch, args)?;
        }
        if wants("auth") {
            crate::watchdog_init::auth(arch, args)?;
        }
        println!("  {arch}: test-watchdog passed");
    }
    Ok(())
}

/// The report's code for user space that stopped feeding its watchdog.
const STARVED_CODE: &str = "  code      FX-9008";
/// The report's headline, up to the opener's process id.
const STARVED: &str = "user space stopped feeding /dev/watchdog: armed by process ";
/// What the headline says of the feeder's timeout, and of the last feed.
const FIVE_SECONDS: &str = "with a 5 s timeout, last fed ";
/// The kernel's line for a value of the option other than `on`.
const NOT_UNDERSTOOD: &str = "watchdog ferrix.watchdog=On is not understood; it is off";
/// The kernel's line once the watchdog is served.
const SERVED: &str = "  watchdog ferrix.watchdog=on: /dev/watchdog is served";
/// Boot 2's loader's quotation of boot 1's report.
const QUOTED: &str = "'s last report: panicked: user space stopped feeding /dev/watchdog";
/// The kernel's line for a last release without the magic `V`.
const STAYS_ARMED: &str = "closed /dev/watchdog without the magic `V`: it stays armed";
/// The kernel's line for the magic close.
const MAGIC_DISARMED: &str = "closed it with the magic `V`: disarmed";
/// The kernel's line for `reboot(2)`'s halt.
const HALT_DISARMED: &str = "  watchdog disarmed for a halt";
/// What the scripts say.
const FEEDING: &str = "wd-feed: feeding";
const STOPPED: &str = "wd-feed: the feeder is stopped";
const SECOND_OPEN: &str = "wd-feed: second open done";
const MAGIC_SENT: &str = "wd-feed: magic close sent";
const STILL: &str = "wd-feed: still running";
const HALTING: &str = "wd-feed: halting";
/// What case 5's program says once it has disarmed and written.
const DISABLED: &str = "watchdog-test: disabled and written";
/// busybox's words for the refusals.
const BUSY: &str = "Resource busy";
const INVALID: &str = "Invalid argument";
/// `L.lockup.6`'s window for the report after the stop line: the timeout
/// less the feed period and the port's slack, and the timeout plus the
/// task's second and a slack of 9 s for a loaded host.
const REPORT_AT_LEAST: Duration = Duration::from_millis(3_500);
const REPORT_AT_MOST: Duration = Duration::from_secs(15);
/// The longest from the report to `ferrix.panic=reset`'s reset.
const RESET_AT_MOST: Duration = Duration::from_secs(60);
/// How long the cases that must not report watch past the feed: the
/// timeout and 5 s.
const QUIET: Duration = Duration::from_secs(10);

/// How the feeder of cases 1 to 3 ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ending {
    /// `SIGSTOP`, the default halt.
    Stopped,
    /// `SIGSTOP` under `ferrix.panic=reset`.
    StoppedReset,
    /// `SIGKILL`: the file's last release, without the magic `V`.
    Killed,
}

/// Cases 1 to 3: the feeder stops feeding; the report must come in the
/// window, and under `ferrix.panic=reset` the machine reset by itself.
///
/// Verifies: H.FAIL.14, `L.lockup.6`, `L.lockup.7`
fn feeder_stops(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
    ending: Ending,
) -> Result<()> {
    let mut options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
    if ending == Ending::StoppedReset {
        options.push_str(" ferrix.panic=reset");
    }
    let signal = if ending == Ending::Killed {
        "KILL"
    } else {
        "STOP"
    };
    let script = format!(
        "#!/bin/sh\nwatchdog -F -T 5 -t 1 /dev/watchdog &\npid=$!\nsleep 3\necho \"{FEEDING} as $pid\"\nkill -{signal} $pid\necho \"{STOPPED}\"\nsleep 600\n"
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
    println!("  {arch}: the feeder ends, {ending:?}");
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let mut report_after = None;
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
                "{arch}: no report within {} s of the feeder's end",
                (REPORT_AT_MOST + PATIENCE).as_secs()
            )));
        }
        report_after = Some(at.elapsed());
        if ending != Ending::StoppedReset {
            // The report halts: give it a moment to finish its lines.
            let _ = watching.read_more(Instant::now() + Duration::from_secs(5), |lines| {
                lines.iter().any(|line| line.contains(STARVED_CODE))
            })?;
            boot_1 = watching
                .lines()
                .iter()
                .chain(watching.after())
                .cloned()
                .collect();
            watching.stop_when_done();
            return Ok(());
        }
        (boot_1, after) = across_the_reset(arch, watching)?;
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
    stopped_problem(&boot_1, &after, report_after, ending).map_or(Ok(()), |problem| {
        Err(Error::new(format!(
            "{arch}: the feeder ended, {ending:?}: {problem}"
        )))
    })?;
    println!(
        "  {arch}: the feeder ended, {ending:?}: FX-9008 {} ms later{}",
        report_after.map_or(0, |taken| taken.as_millis()),
        if ending == Ending::StoppedReset {
            ", the machine reset itself and boot 2's loader quoted the report"
        } else {
            ""
        }
    );
    Ok(())
}

/// Case 2's machine after its report: the loader's banner again within
/// `RESET_AT_MOST`, and its quotation; boot 1's lines and boot 2's.
fn across_the_reset(arch: Arch, watching: &mut Watching<'_>) -> Result<(Vec<String>, Vec<String>)> {
    let seen = watching.after().len();
    let came = watching.read_more(Instant::now() + RESET_AT_MOST + PATIENCE, |lines| {
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
            .any(|line| line.contains(QUOTED))
    })?;
    let boot_1 = watching
        .lines()
        .iter()
        .chain(watching.after().get(..banner).unwrap_or_default())
        .cloned()
        .collect();
    let after = watching.after().get(banner..).unwrap_or_default().to_vec();
    Ok((boot_1, after))
}

/// Why cases 1 to 3 do not show what they must, if they do not.
fn stopped_problem(
    boot_1: &[String],
    after: &[String],
    report_after: Option<Duration>,
    ending: Ending,
) -> Option<String> {
    if !boot_1.iter().any(|line| line.contains(SERVED)) {
        return Some(format!("boot 1's kernel never said `{}...`", SERVED.trim()));
    }
    if ending == Ending::Killed && !boot_1.iter().any(|line| line.contains(STAYS_ARMED)) {
        return Some(format!("the kernel never said `...{STAYS_ARMED}`"));
    }
    let Some(start) = boot_1
        .iter()
        .position(|line| line.contains(qemu::PANIC_MARKER))
    else {
        return Some("no failure report".to_owned());
    };
    let report = boot_1.get(start..).unwrap_or_default();
    let headline = boot_1.get(start).map_or("", |line| line.trim());
    if !report.iter().any(|line| line.contains(STARVED_CODE)) {
        return Some(format!(
            "the report is not the watchdog's: no `{}` line; it began `{headline}`",
            STARVED_CODE.trim()
        ));
    }
    if !(headline.contains(STARVED) && headline.contains(FIVE_SECONDS)) {
        return Some(format!(
            "the headline does not name the opener, its timeout and the last feed: `{headline}`"
        ));
    }
    let report_after = report_after?;
    if report_after < REPORT_AT_LEAST || report_after > REPORT_AT_MOST {
        return Some(format!(
            "the report came {} ms after the feeder's end, outside {} to {} s",
            report_after.as_millis(),
            REPORT_AT_LEAST.as_secs_f32(),
            REPORT_AT_MOST.as_secs()
        ));
    }
    if ending == Ending::StoppedReset {
        if !report.iter().any(|line| line.contains(RESET_LINE)) {
            return Some(format!("the report has no `{}` line", RESET_LINE.trim()));
        }
        if !after.iter().any(|line| line.contains(QUOTED)) {
            return Some(format!("boot 2's loader never said `boot 1{QUOTED}...`"));
        }
    } else if report.iter().any(|line| line.contains(RESET_LINE)) {
        return Some("a report without ferrix.panic=reset announced a reset".to_owned());
    }
    None
}

/// Boot `image` and keep every line until one of `until`, a report or a
/// second loader banner, or `PATIENCE`.
fn watch_until(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    image: &std::path::Path,
    until: &[&str],
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let _ = watching.read_more(Instant::now() + PATIENCE, |lines| {
            lines.iter().any(|line| {
                until.iter().any(|wanted| line.contains(wanted))
                    || line.contains(qemu::PANIC_MARKER)
            }) || lines
                .iter()
                .filter(|line| line.contains(LOADER_BANNER))
                .count()
                > 1
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
        image,
        &parts.kernel.elf,
        args,
        qemu::SUCCESS_MARKER,
        hook,
    )?;
    Ok(lines)
}

/// Case 4: the feeder goes on, a second open is refused, a magic close
/// disarms, a timeout of 0 is refused, and nothing is reported.
fn feed_goes_on(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
    let quiet = QUIET.as_secs();
    let script = format!(
        "#!/bin/sh\n\
         watchdog -F -T 5 -t 1 /dev/watchdog &\n\
         pid=$!\n\
         sleep 1\n\
         echo \"{FEEDING} as $pid\"\n\
         watchdog -F -T 5 /dev/watchdog\n\
         echo \"{SECOND_OPEN}\"\n\
         sleep 19\n\
         kill -TERM $pid\n\
         wait $pid\n\
         echo \"{MAGIC_SENT}\"\n\
         watchdog -F -T 0 -t 1 /dev/watchdog &\n\
         zero=$!\n\
         sleep 1\n\
         kill -TERM $zero\n\
         wait $zero\n\
         sleep {quiet}\n\
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
    println!("  {arch}: the feed goes on for 20 s, then a magic close and {quiet} s more");
    let lines = watch_until(arch, args, parts, &image, &[STILL])?;
    goes_on_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!(
            "{arch}: the feed that goes on: {problem}"
        )))
    })?;
    println!(
        "  {arch}: fed for four timeouts, refused a second open and a timeout of 0, closed with the magic `V`, and no report"
    );
    Ok(())
}

/// A report, or a second loader banner, in `lines`.
fn reported(lines: &[String]) -> Option<String> {
    if let Some(report) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Some(format!("a report came: `{}`", report.trim()));
    }
    (lines
        .iter()
        .filter(|line| line.contains(LOADER_BANNER))
        .count()
        > 1)
    .then(|| "the machine reset".to_owned())
}

/// `wanted`, each after the one before it, in `lines`.
fn in_order(lines: &[String], wanted: &[&str]) -> Option<String> {
    let mut from = 0;
    for each in wanted {
        let Some(at) = lines.iter().skip(from).position(|line| line.contains(each)) else {
            return Some(format!("no `{each}` line after the ones before it"));
        };
        from += at;
    }
    None
}

/// Why case 4 does not show what it must, if it does not.
fn goes_on_problem(lines: &[String]) -> Option<String> {
    reported(lines).or_else(|| {
        in_order(
            lines,
            &[
                FEEDING,
                BUSY,
                SECOND_OPEN,
                MAGIC_DISARMED,
                MAGIC_SENT,
                INVALID,
                STILL,
            ],
        )
    })
}

/// Case 5: disarmed by `WDIOS_DISABLECARD`, then written to and held open:
/// no report.
///
/// Verifies: `L.lockup.7`
fn disabled(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
    let quiet = QUIET.as_secs();
    let script = format!(
        "#!/bin/sh\n\
         /{TEST_PATH} &\n\
         sleep 1\n\
         watchdog -F -T 5 /dev/watchdog\n\
         echo \"{SECOND_OPEN}\"\n\
         sleep {quiet}\n\
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
    println!("  {arch}: DISABLECARD, a write, and {quiet} s held open");
    let lines = watch_until(arch, args, parts, &image, &[STILL])?;
    reported(&lines)
        .or_else(|| {
            in_order(
                &lines,
                &[
                    "asked to stop it: disarmed",
                    DISABLED,
                    BUSY,
                    SECOND_OPEN,
                    STILL,
                ],
            )
        })
        .map_or(Ok(()), |problem| {
            Err(Error::new(format!("{arch}: DISABLECARD: {problem}")))
        })?;
    println!("  {arch}: disarmed by DISABLECARD, a write did not arm it, and no report");
    Ok(())
}

/// Case 6: `reboot(2)`'s halt with the feeder running: disarmed, said.
///
/// Verifies: `L.lockup.8`
fn halted(
    arch: Arch,
    args: &Args,
    parts: &crate::init_file::Parts<'_>,
    shell: &[u8],
) -> Result<()> {
    let options = format!("{} ferrix.watchdog=on", qemu::init_option(STOP_SCRIPT));
    let script = format!(
        "#!/bin/sh\nwatchdog -F -T 5 -t 1 /dev/watchdog &\nsleep 3\necho \"{HALTING}\"\nhalt -f\nsleep 600\n"
    );
    let image = parts.image_with(
        shell,
        STOP_SCRIPT,
        &script,
        &options,
        &["bin/watchdog", "bin/sleep", "bin/halt"],
    )?;
    println!("  {arch}: halt -f with the feeder running");
    let lines = watch_until(arch, args, parts, &image, &[HALT_DISARMED])?;
    reported(&lines)
        .or_else(|| in_order(&lines, &[SERVED, HALTING, HALT_DISARMED]))
        .map_or(Ok(()), |problem| {
            Err(Error::new(format!("{arch}: the halt: {problem}")))
        })?;
    println!("  {arch}: halt -f disarmed the watchdog first and said so");
    Ok(())
}

/// Init's line once it feeds.
const INIT_FEEDS: &str = "feeding /dev/watchdog every ";
/// How long case 7 watches init feed: four of its 5 s timeouts.
const INIT_SECONDS: u64 = 20;

/// Case 7: init feeds under its own option, and nothing is reported.
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
        crate::init::command_line_of(arch).trim_end()
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
    if let Some(problem) = reported(&lines) {
        return Err(Error::new(format!("{arch}: while init fed, {problem}")));
    }
    if fed_for.is_none_or(|fed| fed < Duration::from_secs(INIT_SECONDS)) {
        return Err(Error::new(format!(
            "{arch}: the boot ended before {INIT_SECONDS} s of feeding"
        )));
    }
    println!("  {arch}: init fed /dev/watchdog for {INIT_SECONDS} s, and no report");
    Ok(())
}

/// Case 8: with a value of `ferrix.watchdog` other than `on`, said and
/// taken as off: no node and no task.
///
/// Verifies: `L.lockup.9`
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
        &format!("{} ferrix.watchdog=On", qemu::init_option(STOP_SCRIPT)),
        &["bin/poweroff"],
    )?;
    let lines = watch_until(arch, args, parts, &image, &["wd-off: "])?;
    if !lines.iter().any(|line| line.contains(NOT_UNDERSTOOD)) {
        return Err(Error::new(format!(
            "{arch}: the kernel never said `{NOT_UNDERSTOOD}`"
        )));
    }
    if !lines.iter().any(|line| line.contains("wd-off: absent")) {
        return Err(Error::new(format!(
            "{arch}: with ferrix.watchdog=On, /dev/watchdog was not absent"
        )));
    }
    if lines.iter().any(|line| line.contains(SERVED)) {
        return Err(Error::new(format!(
            "{arch}: with ferrix.watchdog=On, the kernel served the watchdog"
        )));
    }
    println!("  {arch}: ferrix.watchdog=On is not understood: no /dev/watchdog and no task");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    const HEADLINE: &str = "FERRIX-PANIC user space stopped feeding /dev/watchdog: armed by process 7 (watchdog) with a 5 s timeout, last fed 5012 ms ago";

    #[test]
    fn a_report_must_be_the_watchdogs_in_its_window_and_reset_only_when_asked() {
        let boot_1 = lines(&[
            "  watchdog ferrix.watchdog=on: /dev/watchdog is served; once opened",
            HEADLINE,
            "  code      FX-9008  user space stopped feeding its watchdog",
        ]);
        let report = Some(Duration::from_secs(6));
        assert_eq!(stopped_problem(&boot_1, &[], report, Ending::Stopped), None);
        let late = Some(Duration::from_secs(16));
        assert!(
            stopped_problem(&boot_1, &[], late, Ending::Stopped)
                .is_some_and(|why| why.contains("outside 3.5 to 15 s"))
        );
        assert!(
            stopped_problem(&boot_1, &[], report, Ending::Killed)
                .is_some_and(|why| why.contains("stays armed"))
        );
        assert!(
            stopped_problem(&boot_1, &[], report, Ending::StoppedReset)
                .is_some_and(|why| why.contains("reset"))
        );
        let mut reset = boot_1;
        reset.push(RESET_LINE.to_owned());
        let after = lines(&[
            "  pstore   boot 1's last report: panicked: user space stopped feeding /dev/watchdog: armed",
        ]);
        assert_eq!(
            stopped_problem(&reset, &after, report, Ending::StoppedReset),
            None
        );
        assert!(
            stopped_problem(&reset, &after, report, Ending::Stopped)
                .is_some_and(|why| why.contains("announced a reset"))
        );
        let other = lines(&[
            "  watchdog ferrix.watchdog=on: /dev/watchdog is served; once opened",
            "FERRIX-PANIC something else",
            "  code      FX-0001  something else",
        ]);
        assert!(
            stopped_problem(&other, &[], report, Ending::Stopped)
                .is_some_and(|why| why.contains("not the watchdog's"))
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
