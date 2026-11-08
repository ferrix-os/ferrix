//! `cargo xtask test-watchdog`'s cases with init as pid 1
//! (`docs/WATCHDOG-STAGES.md` §8): one hung program is that program's
//! problem, and only a pid 1 that cannot run at all ends the machine.
//!
//! * *service* (W1): `wd-test.service`, `Type=notify` with `WatchdogSec=2s`
//!   and `Restart=on-watchdog`, pings three times and then sleeps. Init must
//!   say its watchdog timeout naming its threads and the call they wait in,
//!   end it with `SIGABRT`, start it again at least twice, and the machine
//!   must report nothing for 40 s while init feeds `/dev/watchdog`;
//! * *blocked* (W3): init's loop is wedged in `open(2)` of a FIFO that a
//!   `svc daemon-reload` finds among the unit files. The feeder must say the
//!   loop has not turned, in `units`, and keep feeding for three timeouts;
//!   a writer then unwedges it, and init says the loop turns again;
//! * *stall* (W3b): the same wedge left in place under
//!   `ferrix.init.watchdog.stall=15`: the stall lines, then the feeder says
//!   the watchdog is no longer fed, and FX-9008 comes within `L.lockup.6`'s
//!   window of that line;
//! * *user* (W2, W4): a user program spinning and one blocked for ever in a
//!   FIFO's read, for 20 s with no report, the spinner's CPU time growing
//!   and the reader's call `read`; then every thread of pid 1 stopped
//!   (`kill -STOP 1`), and FX-9008 within the window. The stop shows the
//!   watchdog was armed and fed all along (the consultant's R-2).
//!
//! Each runs under `ferrix.watchdog=on ferrix.init.watchdog=5`, x86-64.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::postmortem::{PATIENCE, read};
use crate::qemu::{self, Watching};
use crate::{Error, Result};

/// The report's headline, as `watchdog.rs` has it.
const STARVED: &str = "user space stopped feeding /dev/watchdog";
/// Init's line once its feeder feeds.
const FEEDS: &str = "from a thread of its own";
/// The feeder's lines.
const NOT_TURNED: &str = "init: the loop has not turned for ";
const IN_UNITS: &str = "in `units`; the watchdog is still fed";
const TURNS_AGAIN: &str = "init: the loop turns again after ";
const NO_LONGER_FED: &str = "the watchdog is no longer fed";
/// `L.lockup.6`'s window, as `watchdog.rs` measures it.
const REPORT_AT_LEAST: Duration = Duration::from_millis(3_500);
const REPORT_AT_MOST: Duration = Duration::from_secs(15);

/// The applets the scripts call, each a link to busybox.
const APPLETS: [&str; 6] = ["sleep", "mkfifo", "cat", "rm", "cut", "echo"];

/// The driver: a service that runs `script` on the console.
fn driver(script: &str) -> Vec<File> {
    let unit = "[Unit]\nDescription=test-watchdog's driver\n[Service]\nExecStart=/bin/sh /etc/wd-drive.sh\nStandardOutput=console\nStandardError=console\n";
    vec![
        File {
            path: "etc/wd-drive.sh".to_owned(),
            mode: 0o755,
            content: Content::Bytes(script.as_bytes().to_vec()),
        },
        File {
            path: "etc/ferrix/units/wd-drive.service".to_owned(),
            mode: 0o644,
            content: Content::Bytes(unit.as_bytes().to_vec()),
        },
        File {
            path: "etc/ferrix/units/multi-user.target.wants/wd-drive.service".to_owned(),
            mode: 0o777,
            content: Content::Link("../wd-drive.service".to_owned()),
        },
    ]
}

/// An image of init with busybox, the applets, `extra` files and `options`
/// after init's own.
fn image(
    arch: Arch,
    args: &Args,
    extra: Vec<File>,
    options: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let mut files = crate::init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = read(&crate::busybox::program(arch)?)?;
    files.push(File {
        path: "bin/sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox),
    });
    for applet in APPLETS {
        files.push(File {
            path: format!("bin/{applet}"),
            mode: 0o777,
            content: Content::Link("sh".to_owned()),
        });
    }
    files.extend(extra);
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel(arch, args.release)?;
    let natives = crate::native::build(arch, args.release)?;
    let archive = crate::initramfs::build(None, &natives, None, &files)?;
    let options = format!(
        "{} ferrix.watchdog=on ferrix.init.watchdog=5 {options}",
        crate::init::command_line_of(arch).trim_end()
    );
    let image = crate::fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    Ok((image, kernel.elf))
}

/// Boot `image`, let `hook` read, and return every line seen.
fn boot(
    arch: Arch,
    args: &Args,
    image: &std::path::Path,
    kernel: &std::path::Path,
    mut hook: impl FnMut(&mut Watching<'_>) -> Result<()>,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    let _ = qemu::watch_then(
        arch,
        image,
        kernel,
        args,
        qemu::SUCCESS_MARKER,
        |watching| {
            let result = hook(watching);
            lines = watching
                .lines()
                .iter()
                .chain(watching.after())
                .cloned()
                .collect();
            watching.stop_when_done();
            result
        },
    )?;
    Ok(lines)
}

/// Read until a line holds `text`, or `wait` passes: whether it came.
fn until(watching: &mut Watching<'_>, text: &str, wait: Duration) -> Result<bool> {
    watching.read_more(Instant::now() + wait, |lines| {
        lines.iter().any(|line| line.contains(text))
    })
}

/// Read for `wait`, stopping early only at a report: whether one came.
fn quiet_for(watching: &mut Watching<'_>, wait: Duration) -> Result<bool> {
    watching.read_more(Instant::now() + wait, |lines| {
        lines.iter().any(|line| line.contains(qemu::PANIC_MARKER))
    })
}

/// How many lines hold `text`.
fn count(lines: &[String], text: &str) -> usize {
    lines.iter().filter(|line| line.contains(text)).count()
}

/// W1.
pub(crate) fn service(arch: Arch, args: &Args) -> Result<()> {
    let script = "#!/bin/sh\necho READY=1 >&3\necho \"wd-svc: ready\"\nfor i in 1 2 3; do echo WATCHDOG=1 >&3; sleep 1; done\necho \"wd-svc: stops pinging\"\nexec sleep 1000\n";
    let unit = "[Unit]\nDescription=a service that stops answering its watchdog\nStartLimitBurst=20\nStartLimitIntervalSec=10min\n[Service]\nType=notify\nNotifyFd=3\nExecStart=/bin/sh /etc/wd-svc.sh\nWatchdogSec=2s\nTimeoutAbortSec=3s\nRestart=on-watchdog\nRestartSec=1s\nStandardOutput=console\n";
    let files = vec![
        File {
            path: "etc/wd-svc.sh".to_owned(),
            mode: 0o755,
            content: Content::Bytes(script.as_bytes().to_vec()),
        },
        File {
            path: "etc/ferrix/units/wd-test.service".to_owned(),
            mode: 0o644,
            content: Content::Bytes(unit.as_bytes().to_vec()),
        },
        File {
            path: "etc/ferrix/units/multi-user.target.wants/wd-test.service".to_owned(),
            mode: 0o777,
            content: Content::Link("../wd-test.service".to_owned()),
        },
    ];
    let (image, kernel) = image(arch, args, files, "")?;
    println!("  {arch}: a service stops answering its watchdog");
    let lines = boot(arch, args, &image, &kernel, |watching| {
        if !until(watching, "wd-svc: ready", PATIENCE)? {
            return Err(Error::new(format!(
                "{arch}: wd-test.service never said it was ready"
            )));
        }
        let _ = quiet_for(watching, Duration::from_secs(40))?;
        Ok(())
    })?;
    service_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: service case: {problem}")))
    })?;
    println!(
        "  {arch}: the hung service was ended by its watchdog and started {} times, and nothing was reported",
        count(&lines, "wd-svc: ready")
    );
    Ok(())
}

/// Why W1's lines do not show what they must.
fn service_problem(lines: &[String]) -> Option<String> {
    if let Some(report) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Some(format!("a report came: `{}`", report.trim()));
    }
    if count(lines, FEEDS) == 0 {
        return Some(format!("init never said it feeds `{FEEDS}`"));
    }
    let timeouts: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("wd-test.service: watchdog timeout (limit 2000 ms): pid "))
        .collect();
    if timeouts.len() < 2 {
        return Some(String::from(
            "the hung service was never ended by its watchdog (fewer than two timeout reports)",
        ));
    }
    if !timeouts
        .iter()
        .all(|line| line.contains(" threads ") && line.contains("sleep"))
    {
        return Some(format!(
            "a timeout report does not name the thread and its sleep: `{}`",
            timeouts.first().map_or("", |line| line.trim())
        ));
    }
    if count(lines, "wd-svc: ready") < 3 {
        return Some(String::from(
            "the service was not started again at least twice",
        ));
    }
    None
}

/// The driver's wedge: a FIFO among the unit files, then `svc
/// daemon-reload`, which blocks init's loop in its `open(2)`.
const WEDGE: &str = "mkfifo /run/ferrix/units/wedge.service\necho \"wd-drive: wedging init\"\nsvc daemon-reload &\n";

/// W3.
pub(crate) fn blocked(arch: Arch, args: &Args) -> Result<()> {
    let script = format!(
        "#!/bin/sh\nsleep 2\n{WEDGE}sleep 20\necho \"wd-drive: unwedging\"\necho > /run/ferrix/units/wedge.service\nsleep 3\nrm /run/ferrix/units/wedge.service\necho \"wd-drive: done\"\nsleep 1000\n"
    );
    let (image, kernel) = image(arch, args, driver(&script), "")?;
    println!("  {arch}: init's loop is blocked in a read for 20 s");
    let lines = boot(arch, args, &image, &kernel, |watching| {
        if !until(watching, "wd-drive: wedging init", PATIENCE)? {
            return Err(Error::new(format!("{arch}: the driver never wedged init")));
        }
        let _ = watching.read_more(Instant::now() + Duration::from_secs(40), |lines| {
            lines
                .iter()
                .any(|line| line.contains(qemu::PANIC_MARKER) || line.contains("wd-drive: done"))
        })?;
        let _ = quiet_for(watching, Duration::from_secs(5))?;
        Ok(())
    })?;
    blocked_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: blocked case: {problem}")))
    })?;
    println!(
        "  {arch}: init's loop was blocked for 20 s: said, still fed, no report, and it turned again"
    );
    Ok(())
}

/// Why W3's lines do not show what they must.
fn blocked_problem(lines: &[String]) -> Option<String> {
    if let Some(report) = lines.iter().find(|line| line.contains(qemu::PANIC_MARKER)) {
        return Some(format!(
            "a report came while init's loop was blocked: `{}`",
            report.trim()
        ));
    }
    let stuck: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains(NOT_TURNED) && line.contains(IN_UNITS))
        .collect();
    if stuck.len() < 2 {
        return Some(format!(
            "the feeder did not say twice that the loop has not turned, `{IN_UNITS}`"
        ));
    }
    if count(lines, TURNS_AGAIN) == 0 {
        return Some(format!("the feeder never said `{TURNS_AGAIN}...`"));
    }
    if count(lines, "wd-drive: done") == 0 {
        return Some(String::from("the driver never finished"));
    }
    None
}

/// W3b.
pub(crate) fn stall(arch: Arch, args: &Args) -> Result<()> {
    let script = format!("#!/bin/sh\nsleep 2\n{WEDGE}sleep 1000\n");
    let (image, kernel) = image(arch, args, driver(&script), "ferrix.init.watchdog.stall=15")?;
    println!("  {arch}: init's loop is blocked past its stall bound of 15 s");
    let mut report_after = None;
    let lines = boot(arch, args, &image, &kernel, |watching| {
        if !until(watching, "wd-drive: wedging init", PATIENCE)? {
            return Err(Error::new(format!("{arch}: the driver never wedged init")));
        }
        if !until(watching, NO_LONGER_FED, Duration::from_secs(40))? {
            return Ok(());
        }
        let at = Instant::now();
        if until(watching, qemu::PANIC_MARKER, REPORT_AT_MOST + PATIENCE)? {
            report_after = Some(at.elapsed());
            let _ = until(watching, "FX-9008", Duration::from_secs(5))?;
        }
        Ok(())
    })?;
    stall_problem(&lines, report_after).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: stall case: {problem}")))
    })?;
    println!(
        "  {arch}: init's loop stuck past its bound: no longer fed, FX-9008 {} ms later",
        report_after.map_or(0, |taken| taken.as_millis())
    );
    Ok(())
}

/// Why W3b's lines do not show what they must.
fn stall_problem(lines: &[String], report_after: Option<Duration>) -> Option<String> {
    let first_report = lines
        .iter()
        .position(|line| line.contains(qemu::PANIC_MARKER));
    let unfed = lines.iter().position(|line| line.contains(NO_LONGER_FED));
    let Some(unfed) = unfed else {
        return Some(String::from(
            "init's stuck loop was never left unfed: no `the watchdog is no longer fed` line",
        ));
    };
    if first_report.is_some_and(|report| report < unfed) {
        return Some(String::from("a report came before the stall bound"));
    }
    if count(lines, IN_UNITS) == 0 {
        return Some(format!(
            "the feeder never said the loop is stuck `{IN_UNITS}`"
        ));
    }
    let Some(after) = report_after else {
        return Some(String::from("no FX-9008 after the feed stopped"));
    };
    if !lines
        .iter()
        .any(|line| line.contains(STARVED) && line.contains("(init)"))
    {
        return Some(String::from("the report does not name init as the opener"));
    }
    if after < REPORT_AT_LEAST || after > REPORT_AT_MOST {
        return Some(format!(
            "FX-9008 came {} ms after the feed stopped, outside 3.5 to 15 s",
            after.as_millis()
        ));
    }
    None
}

/// W2 and W4.
pub(crate) fn user(arch: Arch, args: &Args) -> Result<()> {
    let script = "#!/bin/sh\nsh -c 'while :; do :; done' &\nspin=$!\nmkfifo /tmp/wd-fifo\ncat <>/tmp/wd-fifo &\nreader=$!\nsleep 2\necho \"wd-drive: spin at $(cut -d' ' -f14 /proc/$spin/stat)\"\necho \"wd-drive: reader in $(cat /proc/$reader/wchan)\"\nsleep 20\necho \"wd-drive: spin now $(cut -d' ' -f14 /proc/$spin/stat)\"\necho \"wd-drive: stopping init\"\nkill -STOP 1\nsleep 1000\n";
    let (image, kernel) = image(arch, args, driver(script), "")?;
    println!("  {arch}: a user program spins and one blocks, for 20 s; then pid 1 is stopped");
    let mut report_after = None;
    let lines = boot(arch, args, &image, &kernel, |watching| {
        if !until(watching, "wd-drive: spin at", PATIENCE)? {
            return Err(Error::new(format!(
                "{arch}: the driver never started its programs"
            )));
        }
        if !watching.read_more(Instant::now() + Duration::from_secs(40), |lines| {
            lines.iter().any(|line| {
                line.contains(qemu::PANIC_MARKER) || line.contains("wd-drive: stopping init")
            })
        })? {
            return Ok(());
        }
        let at = Instant::now();
        if until(watching, qemu::PANIC_MARKER, REPORT_AT_MOST + PATIENCE)? {
            report_after = Some(at.elapsed());
            let _ = until(watching, "FX-9008", Duration::from_secs(5))?;
        }
        Ok(())
    })?;
    user_problem(&lines, report_after).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: user case: {problem}")))
    })?;
    println!(
        "  {arch}: a spinning and a blocked program for 20 s, no report; pid 1 stopped, FX-9008 {} ms later",
        report_after.map_or(0, |taken| taken.as_millis())
    );
    Ok(())
}

/// The number after `prefix` in the line that holds it.
fn number_after(lines: &[String], prefix: &str) -> Option<u64> {
    lines.iter().find_map(|line| {
        let at = line.find(prefix)? + prefix.len();
        line.get(at..)?.split_whitespace().next()?.parse().ok()
    })
}

/// Why W2 and W4's lines do not show what they must.
fn user_problem(lines: &[String], report_after: Option<Duration>) -> Option<String> {
    let stop = lines
        .iter()
        .position(|line| line.contains("wd-drive: stopping init"));
    let report = lines
        .iter()
        .position(|line| line.contains(qemu::PANIC_MARKER));
    let Some(stop) = stop else {
        return Some(match report {
            Some(_) => String::from("a report came while user programs hung"),
            None => String::from("the driver never stopped init"),
        });
    };
    if report.is_some_and(|report| report < stop) {
        return Some(String::from("a report came while user programs hung"));
    }
    if count(lines, FEEDS) == 0 {
        return Some(format!("init never said it feeds `{FEEDS}`"));
    }
    let before = number_after(lines, "wd-drive: spin at ");
    let after = number_after(lines, "wd-drive: spin now ");
    if !matches!((before, after), (Some(before), Some(after)) if after > before) {
        return Some(format!(
            "the spinner's CPU time did not grow ({before:?} then {after:?})"
        ));
    }
    if !lines
        .iter()
        .filter_map(|line| {
            line.split_once("wd-drive: reader in ")
                .map(|(_, call)| call)
        })
        .any(|call| call.starts_with("read"))
    {
        return Some(String::from("the FIFO's reader was not in a read"));
    }
    match report_after {
        None => Some(String::from("no FX-9008 after pid 1 was stopped")),
        Some(after) if after < REPORT_AT_LEAST || after > REPORT_AT_MOST => Some(format!(
            "FX-9008 came {} ms after pid 1 was stopped, outside 3.5 to 15 s",
            after.as_millis()
        )),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn the_service_case_wants_two_timeouts_naming_the_sleep_and_no_report() {
        let timeout = "  init     wd-test.service: watchdog timeout (limit 2000 ms): pid 80 threads sleep/80 clock_nanosleep";
        let good = lines(&[
            "  init     feeding /dev/watchdog every 1666 ms from a thread of its own, its timeout 5 s",
            "wd-svc: ready",
            timeout,
            "wd-svc: ready",
            timeout,
            "wd-svc: ready",
        ]);
        assert_eq!(service_problem(&good), None);
        let mut reported = good.clone();
        reported.push("FERRIX-PANIC user space stopped feeding /dev/watchdog".to_owned());
        assert!(service_problem(&reported).is_some_and(|why| why.contains("report")));
        assert!(service_problem(&good[..3]).is_some_and(|why| why.contains("never ended")));
    }

    #[test]
    fn the_stall_case_wants_the_report_in_its_window_after_the_unfed_line() {
        let good = lines(&[
            "init: the loop has not turned for 5 s, in `units`; the watchdog is still fed",
            "init: the loop has not turned for 15 s, in `units`; the watchdog is no longer fed",
            "FERRIX-PANIC user space stopped feeding /dev/watchdog: armed by process 1 (init) with a 5 s timeout",
        ]);
        assert_eq!(stall_problem(&good, Some(Duration::from_secs(6))), None);
        assert!(stall_problem(&good, Some(Duration::from_secs(20))).is_some());
        assert!(
            stall_problem(&good[..1], None).is_some_and(|why| why.contains("never left unfed"))
        );
    }

    #[test]
    fn the_user_case_wants_growth_a_read_and_the_stop_reported() {
        let good = lines(&[
            "  init     feeding /dev/watchdog every 1666 ms from a thread of its own, its timeout 5 s",
            "wd-drive: spin at 150",
            "wd-drive: reader in read:pipe:[4]",
            "wd-drive: spin now 2100",
            "wd-drive: stopping init",
            "FERRIX-PANIC user space stopped feeding /dev/watchdog",
        ]);
        assert_eq!(user_problem(&good, Some(Duration::from_secs(6))), None);
        let mut early = good;
        early.insert(2, "FERRIX-PANIC user space stopped feeding".to_owned());
        assert!(user_problem(&early, Some(Duration::from_secs(6))).is_some());
    }
}
