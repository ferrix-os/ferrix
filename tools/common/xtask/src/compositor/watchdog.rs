//! `test-compositor --boot watchdog` (`docs/WATCHDOG-STAGES.md` §5.5, §8):
//! `hyprix.service`'s watchdog is report-only. hyprix pings it from its
//! loop; a script hyprix starts stops it (`SIGSTOP`) for 20 s, twice its
//! 10 s `WatchdogSec=`, and continues it. Init must say the timeout with
//! hyprix's threads, leave it running (`WatchdogAction=report`), and say it
//! answers again once continued; the compositor must not be killed or
//! started again, and the kernel must not stop.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::boot::{build_image, say_the_marker, undithered};
use super::{Carried, EITHER, FAILED, MARKER, Programs, gates_busybox};
use crate::args::Args;
use crate::paths::Arch;
use crate::qemu::Watching;
use crate::{Error, Result};

/// The compositor's configuration: the script, started by hyprix, so its
/// parent is the compositor.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot watchdog`.
exec-once = /bin/sh /etc/wd-hyprix.sh
";

/// The script: stop the compositor past its watchdog, then continue it.
const SCRIPT: &str = "#!/bin/sh
sleep 5
echo \"wd-hyprix: stopping the compositor $PPID\" > /dev/console
kill -STOP $PPID
sleep 20
echo \"wd-hyprix: continuing it\" > /dev/console
kill -CONT $PPID
sleep 10
echo \"wd-hyprix: done\" > /dev/console
";

/// What init says.
const TIMEOUT: &str = "hyprix.service: watchdog timeout (limit 10000 ms): pid ";
const LEFT_RUNNING: &str = "hyprix.service: WatchdogAction=report: left running";
const ANSWERS: &str = "hyprix.service: answers its watchdog again";
/// What hyprix says once it pings.
const PINGS: &str = "hyprix: its loop pings hyprix.service's watchdog";

/// The boot.
pub(super) fn test_watchdog(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let Some(busybox) = gates_busybox(arch) else {
        println!("  {arch}: no static busybox for the script that stops the compositor; skipped");
        return Ok(());
    };
    let carried = Carried {
        busybox: Some(PathBuf::from(busybox)),
        ports: vec![crate::ports::File {
            path: "etc/wd-hyprix.sh".to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(SCRIPT.as_bytes().to_vec()),
        }],
        ..Carried::none()
    };
    let (image, kernel) = build_image(arch, programs, &undithered(CONFIG), carried, args)?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        let up = watching.read_more(Instant::now() + Duration::from_secs(120), |lines| {
            lines.iter().any(|line| line.contains(MARKER))
        })?;
        if let Some(line) = watching.lines().iter().find(|line| line.contains(FAILED)) {
            return Err(Error::new(format!("{arch}: {}", line.trim())));
        }
        if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
            return Err(Error::new(format!(
                "{arch}: the compositor never printed `{MARKER}`"
            )));
        }
        say_the_marker(watching, arch);
        let _ = watching.read_more(Instant::now() + Duration::from_secs(90), |lines| {
            lines
                .iter()
                .any(|line| line.contains("wd-hyprix: done") || line.contains("FERRIX-PANIC"))
        })?;
        watching.read_what_was_said(Duration::from_secs(2))?;
        said = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    problem(&said).map_or(Ok(()), |why| {
        Err(Error::new(format!("{arch}: watchdog boot: {why}")))
    })?;
    println!(
        "  {arch}: the compositor stopped for 20 s was reported by its watchdog with its threads, \
         left running, and answered again once continued"
    );
    Ok(())
}

/// Why the transcript does not show what it must.
fn problem(said: &[String]) -> Option<String> {
    let at = |text: &str| said.iter().position(|line| line.contains(text));
    if let Some(line) = said.iter().find(|line| line.contains("FERRIX-PANIC")) {
        return Some(format!("the kernel stopped: {}", line.trim()));
    }
    if at(PINGS).is_none() {
        return Some(format!("hyprix never said `{PINGS}`"));
    }
    let Some(stop) = at("wd-hyprix: stopping the compositor") else {
        return Some(String::from("the script never stopped the compositor"));
    };
    let Some(timeout) = at(TIMEOUT) else {
        return Some(String::from(
            "the stopped compositor was never reported by its watchdog",
        ));
    };
    if timeout < stop {
        return Some(String::from(
            "a watchdog timeout came before the compositor was stopped",
        ));
    }
    if at(LEFT_RUNNING).is_none() {
        return Some(format!("init never said `{LEFT_RUNNING}`"));
    }
    let Some(answers) = at(ANSWERS) else {
        return Some(String::from(
            "the continued compositor never answered its watchdog again",
        ));
    };
    if answers < timeout {
        return Some(String::from("it answered before the timeout was said"));
    }
    if at("wd-hyprix: done").is_none() {
        return Some(String::from("the script never finished"));
    }
    let restarted = said.iter().skip(stop).any(|line| {
        line.contains("hyprix.service: watchdog;")
            || line.contains("hyprix.service: stopped")
            || line.contains("hyprix.service: failed")
    });
    if restarted {
        return Some(String::from("the compositor was stopped or restarted"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::problem;

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn a_report_left_running_and_answered_passes() {
        let good = lines(&[
            "hyprix: its loop pings hyprix.service's watchdog",
            "wd-hyprix: stopping the compositor 80",
            "  init     hyprix.service: watchdog timeout (limit 10000 ms): pid 80 threads hyprix/80 running",
            "  init     hyprix.service: WatchdogAction=report: left running",
            "wd-hyprix: continuing it",
            "  init     hyprix.service: answers its watchdog again, 9000 ms after it was due",
            "wd-hyprix: done",
        ]);
        assert_eq!(problem(&good), None);
        let mut killed = good.clone();
        killed.push("  init     hyprix.service: watchdog; restarting at 50s".to_owned());
        assert!(problem(&killed).is_some());
        assert!(problem(&good[..2]).is_some_and(|why| why.contains("never reported")));
    }
}
