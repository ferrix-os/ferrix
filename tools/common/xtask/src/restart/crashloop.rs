//! `test-restart --boot crashloop`: a driver that dies at every start is
//! restarted twice with growing delays, started once more on a
//! reinitialised device, and then its device is quarantined
//! (`docs/DEVMGR.md` §4, the crash loop's escalation).
//!
//! It is the N150 box's boot of 2026-10-30 on QEMU: the Intel GPU's driver
//! died at every start. The image carries the stand-in for ADL-N's GPU at
//! 00:02.0 (`intel_uapi::STANDIN`) and, under the name `intel`,
//! `crash-at-start` (`src/user/system/native/crash-at-start`): a driver
//! that pins one page to its device, says so with the time, and exits 70.
//! No `intel-gt` is carried, so devmgr starts it as a plain engine, taken
//! at its word as the box's driver was. The init reads `devmgr-events`
//! with `read-events` until a `quarantined` line, then waits out the time a
//! further start would take.
//!
//! The judge wants, in order: four starts that each pinned their page, the
//! gaps between them at least 1, 2 and 4 s; devmgr's restarts 1 and 2, its
//! reinitialisation, restart 3 and its quarantine line; on `devmgr-events`
//! died, restarted 1, died, restarted 2, died, reinit 3, restarted 3, died,
//! quarantined 3; the kernel's word that the dead drivers' pins are kept;
//! and no fifth start, no pin refused past twice the device's budget.
//!
//! Its negative control maps `Next::Quarantine` to a restart in
//! `ferrix-restart`: the device is never quarantined, and the judge says so.

use crate::args::Args;
use crate::native::{self, Program};
use crate::paths::{self, Arch};
use crate::zinc;
use crate::{Error, Result, busybox, cargo, fat, initramfs, intel_uapi, qemu, shell};

/// The driver that dies at every start.
const CRASHER: Program = Program {
    package: "ferrix-crash-at-start",
    binary: "crash-at-start",
    directory: native::DRIVERS,
};

/// Where the events reader is carried.
const READER_PATH: &str = "bin/read-events";

/// The init: read the events until the quarantine, then wait past the time
/// a further start would come, 8 s after the last death.
const SCRIPT: &str = r#"export PATH=/bin HOME=/tmp
/bin/read-events quarantined 90
echo "crashloop-gate: read-events exited $?"
busybox sleep 10
echo "crashloop-gate: waited"
exit 17
"#;

/// What the script exits with when it ran to its end.
const STATUS: &str = "17";

/// What `crash-at-start` says at each start, and of its pin.
const STARTED: &str = "crash-at-start: 0x00000010 at ";
const PINNED: &str = "pinned 1 page; dying at my start";

/// The kernel's lines for devmgr's DIED and RESTARTED.
const DIED: &str = "devmgr   the driver of 0x00000010 ended with status 70";
const RESTARTED: &str =
    "devmgr   the driver of 0x00000010 was started again and published (restart";

/// devmgr's own lines.
const REINIT: &str = "the device is reinitialised";
const QUARANTINED: &str = "intel 0x00000010 quarantined after 3 restarts";

/// The kernel's lines about the dead drivers' pins.
const KEPT: &str = "a dead driver's pins are kept, mapped";
const FULL: &str = "past twice its pin budget";

/// What `read-events` prints before each line.
const EVENT: &str = "devmgr-event: ";

/// The events, in order.
const EVENTS: [&str; 9] = [
    "died intel 0x00000010 70",
    "restarted intel 0x00000010 1",
    "died intel 0x00000010 70",
    "restarted intel 0x00000010 2",
    "died intel 0x00000010 70",
    "reinit intel 0x00000010 3",
    "restarted intel 0x00000010 3",
    "died intel 0x00000010 70",
    "quarantined intel 0x00000010 3",
];

/// The least gap, in milliseconds, before each start after the first:
/// devmgr's delays, 1, 2 and 4 s.
const GAPS: [u64; 3] = [1000, 2000, 4000];

/// Build the static events reader with the host's cc.
fn reader(dir: &std::path::Path) -> Result<Vec<u8>> {
    let source =
        paths::workspace_root().join("src/user/system/native/devmgr-events/test/read-events.c");
    let out = dir.join("read-events");
    let built = std::process::Command::new("cc")
        .args(["-O2", "-Wall", "-Wextra", "-Werror", "-static", "-o"])
        .arg(&out)
        .arg(&source)
        .status()
        .map_err(|error| Error::new(format!("running cc: {error}")))?;
    if !built.success() {
        return Err(Error::new(format!(
            "building {} failed ({built}); it needs the host's static libc",
            source.display()
        )));
    }
    std::fs::read(&out).map_err(|error| Error::new(format!("reading {}: {error}", out.display())))
}

/// The test.
///
/// # Errors
///
/// An unpatched QEMU, a build or boot failure, or a judge that did not find
/// what it wants.
pub(crate) fn test(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    let mut args = args.clone();
    intel_uapi::refuse_unpatched_qemu()?;
    args.intel_gpu_standin = true;
    if !args.timeout_given {
        args.timeout = 240;
    }
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    println!("  {arch}: building an image whose intel driver dies at every start");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &shell, SCRIPT)?;
    let mut natives = native::build(arch, args.release)?;
    let crasher = native::build_one(arch, args.release, &paths::target_dir(), &CRASHER)?;
    let slot = natives
        .iter_mut()
        .find(|built| built.name == "intel")
        .ok_or_else(|| Error::new("the native programs carry no intel driver to replace"))?;
    slot.bytes = crasher.bytes;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    let busybox = busybox::program(arch)?;
    let dir = paths::build_dir(arch).join("crashloop");
    std::fs::create_dir_all(&dir)
        .map_err(|error| Error::new(format!("making {}: {error}", dir.display())))?;
    let files = vec![crate::ports::File {
        path: READER_PATH.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(reader(&dir)?),
    }];
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let options = format!("{}\n", intel_uapi::OPTION);
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!(
        "  {arch}: the Intel driver dies at every start (timeout {}s)",
        args.timeout
    );
    let lines = qemu::watch_then(arch, &image, &kernel, &args, shell::EXITED, |_| Ok(()))?;
    let log = paths::build_dir(arch).join("serial.log");
    let kept = dir.join("serial.log");
    let _ = std::fs::copy(&log, &kept);
    let gaps = judge(&lines).map_err(|why| {
        Error::new(format!(
            "{arch}: {why}.\n  Serial output is in {}",
            kept.display()
        ))
    })?;
    println!(
        "  {arch}: restarted twice, reinitialised, then quarantined; starts {gaps:?} ms apart"
    );
    Ok(())
}

/// What a run must show; the gaps between the starts, or the first thing
/// missing, said.
fn judge(lines: &[String]) -> std::result::Result<Vec<u64>, String> {
    let exited = lines
        .iter()
        .find_map(|line| line.trim().split(shell::EXITED).nth(1))
        .map(str::trim);
    if exited != Some(STATUS) {
        let said = lines
            .iter()
            .rev()
            .find(|line| line.contains("crashloop-gate: ") || line.contains(EVENT))
            .map_or("", |line| line.trim());
        return Err(format!(
            "the script ended with {exited:?}; its last word: `{said}`"
        ));
    }
    if let Some(line) = lines.iter().find(|line| line.contains(FULL)) {
        return Err(format!(
            "a pin was refused past twice the device's budget: `{}`",
            line.trim()
        ));
    }
    let starts: Vec<&String> = lines.iter().filter(|line| line.contains(STARTED)).collect();
    if starts.len() != 4 {
        return Err(format!(
            "{} starts of the driver, not 4: the first, two restarts and the reinitialised one",
            starts.len()
        ));
    }
    if let Some(line) = starts.iter().find(|line| !line.contains(PINNED)) {
        return Err(format!("a start did not pin its page: `{}`", line.trim()));
    }
    let times: Vec<u64> = starts
        .iter()
        .filter_map(|line| {
            line.split(STARTED)
                .nth(1)?
                .split(" ms")
                .next()?
                .trim()
                .parse()
                .ok()
        })
        .collect();
    let gaps: Vec<u64> = times
        .iter()
        .zip(times.iter().skip(1))
        .map(|(earlier, later)| later.saturating_sub(*earlier))
        .collect();
    if gaps.len() != GAPS.len() {
        return Err(format!("the starts' times did not read: {times:?}"));
    }
    for (index, (gap, least)) in gaps.iter().zip(GAPS).enumerate() {
        if *gap < least {
            return Err(format!(
                "start {} came {gap} ms after the one before, under devmgr's {least} ms",
                index + 2
            ));
        }
    }
    let count = |want: &str| lines.iter().filter(|line| line.contains(want)).count();
    if count(DIED) != 4 {
        return Err(format!("{} DIED lines, not 4", count(DIED)));
    }
    for restart in 1..=3 {
        let want = format!("{RESTARTED} {restart})");
        if count(&want) != 1 {
            return Err(format!("no single line `{want}`"));
        }
    }
    for (want, what) in [
        (REINIT, "devmgr's reinitialisation"),
        (QUARANTINED, "devmgr's quarantine"),
        (KEPT, "the kernel keeping the dead drivers' pins"),
    ] {
        if count(want) == 0 {
            return Err(format!("no line showed {what} (`{want}`)"));
        }
    }
    let events: Vec<&str> = lines
        .iter()
        .filter_map(|line| line.split(EVENT).nth(1))
        .map(str::trim)
        .collect();
    if events != EVENTS {
        return Err(format!("devmgr-events said {events:?}, not {EVENTS:?}"));
    }
    Ok(gaps)
}

#[cfg(test)]
mod tests {
    use super::{EVENTS, judge};

    /// A run as the gate wants it.
    fn good() -> Vec<String> {
        let mut lines = vec![];
        let mut at = 1000;
        for (start, gap) in [0, 1100, 2100, 4100].into_iter().enumerate() {
            at += gap;
            lines.push(format!(
                "crash-at-start: 0x00000010 at {at} ms: pinned 1 page; dying at my start"
            ));
            lines.push(
                "  devmgr   the driver of 0x00000010 ended with status 70; the device is quiesced"
                    .to_owned(),
            );
            if start == 0 {
                lines.push(
                    "  iommu    a dead driver's pins are kept, mapped, until its device's next driver has reset it"
                        .to_owned(),
                );
            }
            if start == 2 {
                lines.push("devmgr   intel 0x00000010: 2 restarts in a row died within 10 s of their start; the device is reinitialised -- quiesced again and its configuration checked -- and the driver started once more in 4000 ms".to_owned());
            }
            if start < 3 {
                lines.push(format!(
                    "  devmgr   the driver of 0x00000010 was started again and published (restart {})",
                    start + 1
                ));
            }
        }
        lines.push("devmgr   intel 0x00000010 quarantined after 3 restarts: the start on the reinitialised device died too.".to_owned());
        for event in EVENTS {
            lines.push(format!("devmgr-event: {event}"));
        }
        lines.push("init     the shell exited with 17".to_owned());
        lines
    }

    #[test]
    fn a_good_run_passes() {
        assert_eq!(judge(&good()), Ok(vec![1100, 2100, 4100]));
    }

    #[test]
    fn a_restart_too_soon_fails() {
        let lines: Vec<String> = good()
            .into_iter()
            .map(|line| line.replace("at 4200 ms", "at 2500 ms"))
            .collect();
        assert!(judge(&lines).unwrap_err().contains("start 3 came"));
    }

    /// The negative control's run: no quarantine, so a fifth start, and the
    /// reader timed out.
    #[test]
    fn a_device_never_quarantined_fails() {
        let mut lines: Vec<String> = good()
            .into_iter()
            .filter(|line| !line.contains("quarantined"))
            .collect();
        lines.push(
            "crash-at-start: 0x00000010 at 16300 ms: pinned 1 page; dying at my start".to_owned(),
        );
        assert!(judge(&lines).unwrap_err().contains("5 starts"));
    }

    #[test]
    fn a_pin_refused_past_the_quarantine_fails() {
        let mut lines = good();
        lines.push("  iommu    00:02.0: a pin of 1 pages was refused, past twice its pin budget, counting its quarantine: x".to_owned());
        assert!(judge(&lines).unwrap_err().contains("past twice"));
    }
}
