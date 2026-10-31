//! `cargo xtask test-postmortem`: the kernel's log kept across a reset
//! (`docs/POSTMORTEM.md` §3.7).
//!
//! **The record.** One QEMU, kept across a reset: no `-no-reboot`, a power-off
//! only pauses it, and a QMP port. The image's command line says
//! `ferrix.onexit=panic`, so boot 1 runs to init's exit and panics on purpose
//! (`FX-1501`). Once the report says the region is written back, the gate
//! saves the region's RAM over QMP and resets the machine, as a warm reset
//! does: RAM kept, firmware and the loader started again. It requires
//!
//! 1. boot 1's loader to have found nothing there (a cold boot), and its
//!    kernel to mirror the log to the region the loader named;
//! 2. the region saved before the reset to hold boot 1's log: a valid header
//!    of boot 1, and its text boot 1's serial log line for line, less the
//!    lines the kernel keeps out of its log (`L.console.42`), and none of
//!    those;
//! 3. the loader a second time, its `pstore` lines naming boot 1's log at the
//!    same address and quoting boot 1's panic line exactly as the loader
//!    renders it, and boot 2's kernel mirroring boot 2.
//!
//! **The switch.** A second QEMU of an image whose command line says
//! `ferrix.pstore=off`: the loader takes no region and says so, and the
//! kernel has none.
//!
//! **The reset** (`ferrix.panic=reset`, the stop-gap of
//! `src/kernel/src/panic/reset.rs`). A third QEMU, kept across a reset as the
//! first, whose command line adds `ferrix.panic=reset` and which nobody
//! resets: boot 1's report must say the reset is coming, the machine must
//! reset by itself no sooner than the wait and not much later, and boot 2's
//! loader must quote boot 1's panic. The first QEMU, without the option, is
//! the default's evidence: its report says the processor halts, and no reset
//! line. A fourth, with `ferrix.panic=Reset`, shows a misspelt value
//! said as not understood and taken as halt.
//!
//! **The stops** (`docs/POSTMORTEM.md` §5). One QEMU for each way the kernel
//! stops on purpose but a panic, kept across a reset as the first: a program's
//! `reboot(2)` RESTART, RESTART2, `POWER_OFF` and HALT, and init's exit under
//! `ferrix.onexit=reset` and under the default. A power-off or a halt only
//! pauses QEMU, which the gate resets over QMP. Boot 2's loader must say how
//! boot 1 ended, and boot 2's kernel attach the fresh header. One more QEMU
//! is reset over QMP while its program sleeps, and boot 2's loader must say
//! that no stop was recorded. The record's case requires `a panic`, in the
//! saved region's word and in boot 2's loader's line.
//!
//! **The event log** (`docs/POSTMORTEM.md` §4, x86-64). The image's ESP in a
//! GUID partition table laid out as the N150 box's disk is, with the event
//! log's partition at LBA 40-2047 before it. Boot 1 panics and resets by
//! itself; boot 2's loader keeps the record in the log before it lets the
//! region go; then the power is cut (the QEMU process ends) and a second
//! process boots the disk cold: boot 3's loader finds no pstore and quotes
//! boot 1's panic from the log, and the host reads the same record back
//! through `ferrix-eventlog`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrix_bootinfo::pstore::{self, Ended, HEADER_BYTES, Stop, Text};

use crate::args::Args;
use crate::display::Qmp;
use crate::paths::{self, Arch};
use crate::qemu::{self, Watching};
use crate::task::Task;
use crate::{Error, Result};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-postmortem",
    help_at: 500,
    help: &[
        "test-postmortem  the kernel's log across a warm reset: a panic, a QMP reset, the next loader quotes it; ferrix.pstore=off; ferrix.panic=reset (x86_64, aarch64)",
    ],
    run: |_, args| test_postmortem(args, crate::build_image),
}];

/// The kernel's line once the region is attached.
const MIRRORING: &str = "pstore   mirroring the log to ";
/// The panic report's line once the region is written back: unlogged, the
/// last the report sends before the screen.
const WRITTEN_BACK: &str = "pstore    written back for the next boot";
/// The loader's lines.
const GOES_TO: &str = "'s kernel log goes to ";
const NONE: &str = "no region this boot";
const LAST_REPORT: &str = "'s last report: panicked: ";
/// The loader's banner, which a reset brings back.
const LOADER_BANNER: &str = "Ferrix loader ";
/// The report's line under `ferrix.panic=reset`, before the wait.
const RESET_LINE: &str = "  reset     in 10 s, as ferrix.panic=reset asks";
/// The report's `stopped` line under `ferrix.panic=reset`, and without it.
const STOPPED_RESETS: &str = "  stopped   this processor resets the machine after the report";
const STOPPED_HALTS: &str = "  stopped   this processor halts here";
/// The wait between the reset line and the reset, less the slack a host
/// reading the serial port may add; and the longest a reset may take after
/// the wait, firmware included.
const RESET_WAIT_AT_LEAST: Duration = Duration::from_millis(9_500);
const RESET_WAIT_AT_MOST: Duration = Duration::from_secs(60);
/// How long each step waits.
const PATIENCE: Duration = Duration::from_secs(120);
/// The longest quotation of a previous kernel's line the loader prints.
const SHOWN: usize = 160;

/// Lines the kernel sends to the port and keeps out of its log
/// (`console::write_unlogged`), each by its start and a piece of its format:
/// the region must hold none of them, and they are the only lines of boot
/// 1's serial log it may lack.
const UNLOGGED: [(&str, &str); 7] = [
    ("  frames   ", " MiB managed, "),
    ("  kernel   ", " -> "),
    ("  physmap  ", " covering "),
    ("  kaslr     slide ", "0x"),
    ("  trace     #", "0x"),
    ("  pstore    written back", " for the next boot"),
    ("  log      a line sent unlogged", "not in the log"),
];

/// Whether `line` is one the kernel sends unlogged.
fn unlogged(line: &str) -> bool {
    UNLOGGED
        .iter()
        .any(|(start, part)| line.starts_with(start) && line.contains(part))
}

/// `cargo xtask test-postmortem`, for each architecture asked: the
/// record across a reset, then the switch.
pub(crate) fn test_postmortem(
    args: &Args,
    build: impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    for arch in args.arches()? {
        if arch == Arch::Armv7a {
            return Err(Error::new(
                "test-postmortem runs on x86_64 and aarch64: the ARMv7-A board boots \
                 through U-Boot, whose RAM across a reset is not this gate's to claim",
            ));
        }
        across_a_reset(arch, args, &build)?;
        switched_off(arch, args, &build)?;
        reset_by_itself(arch, args, &build)?;
        misspelt(arch, args, &build)?;
        stops(arch, args, &build)?;
        if arch == Arch::X86_64 {
            kept_across_power(arch, args, &build)?;
        } else {
            println!("  {arch}: the event log's power-cycle case runs on x86_64");
        }
        println!("  {arch}: test-postmortem passed");
    }
    Ok(())
}

/// The record: boot 1 panics, the machine resets, boot 2's loader reports it.
fn across_a_reset(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut wanted = args.clone();
    wanted.kernel_options.push("ferrix.onexit=panic".to_owned());
    wanted.keep_ram = true;
    let port = crate::display::free_port()?;
    wanted.qmp_port = Some(port);
    let (image, kernel) = build(arch, &wanted)?;
    let saved = paths::build_dir(arch).join("pstore-region.bin");
    let _ = std::fs::remove_file(&saved);
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let written = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(WRITTEN_BACK))
        })?;
        if !written {
            return Err(Error::new(format!(
                "boot 1's report never said `{WRITTEN_BACK}`"
            )));
        }
        boot_1 = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        let address = region_address(&boot_1)?;
        let mut qmp = Qmp::connect(port, deadline)?;
        let _ = qmp.execute(
            "pmemsave",
            Some(&format!(
                "{{\"val\":{address},\"size\":{},\"filename\":{}}}",
                pstore::LEN,
                crate::display::json_string(&saved.display().to_string())
            )),
        )?;
        println!("  {arch}: region saved, resetting the machine as a warm reset does");
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
    let _ = qemu::watch_then(arch, &image, &kernel, &wanted, qemu::PANIC_MARKER, hook)?;
    let region = std::fs::read(&saved)
        .map_err(|error| Error::new(format!("reading {}: {error}", saved.display())))?;
    judge(arch, &boot_1, &region, &after)?;
    let disk = read(&image)?;
    if let Some(problem) = kept_file_problem(&disk, &region, &after) {
        return Err(Error::new(format!("{arch}: {problem}")));
    }
    println!("  {arch}: boot 2's loader kept boot 1's log in FERRIX/LASTKERNEL.TXT on the ESP");
    Ok(())
}

/// Why the ESP's `FERRIX/LASTKERNEL.TXT`, which boot 2's loader wrote, is
/// not the report lines it printed followed by boot 1's region text, if it
/// is not (`docs/POSTMORTEM.md` §3.5). Read with `ferrix-fat`, the reader
/// `lode` imports it with.
fn kept_file_problem(disk: &[u8], region: &[u8], after: &[String]) -> Option<String> {
    let mut volume = match ferrix_fat::Volume::open(disk) {
        Ok(volume) => volume,
        Err(why) => return Some(format!("the image's ESP does not read as FAT32: {why:?}")),
    };
    let file = match volume.read_file("FERRIX/LASTKERNEL.TXT", 4 << 20) {
        Ok(file) => file,
        Err(why) => return Some(format!("FERRIX/LASTKERNEL.TXT does not read: {why:?}")),
    };
    let header: &[u8; HEADER_BYTES] = region.get(..HEADER_BYTES)?.try_into().ok()?;
    let found = pstore::validate(header, region.len() as u64).ok()?;
    let ring = region.get(pstore::HEADER as usize..)?;
    let (first, second) = Text::of(ring, &found).parts();
    let mut text = first.to_vec();
    text.extend_from_slice(second);
    let Some(head) = file
        .len()
        .checked_sub(text.len())
        .and_then(|at| file.get(..at))
    else {
        return Some(format!(
            "FERRIX/LASTKERNEL.TXT holds {} bytes, fewer than boot 1's log's {}",
            file.len(),
            text.len()
        ));
    };
    if file.get(head.len()..) != Some(text.as_slice()) {
        return Some(
            "FERRIX/LASTKERNEL.TXT does not end with boot 1's log as the region held it".into(),
        );
    }
    let head = String::from_utf8_lossy(head);
    let said: Vec<&str> = after
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| line.contains("  pstore   ") && line.contains("boot 1"))
        .filter(|line| !line.contains("LASTKERNEL"))
        .collect();
    for line in head.lines() {
        if !said.iter().any(|each| each.ends_with(line.trim_end())) {
            return Some(format!(
                "FERRIX/LASTKERNEL.TXT's head line `{line}` is not one boot 2's loader printed"
            ));
        }
    }
    if !head.contains("last report: panicked:") {
        return Some("FERRIX/LASTKERNEL.TXT's head does not quote boot 1's panic".into());
    }
    None
}

/// The physical address boot 1's kernel said it mirrors to.
fn region_address(lines: &[String]) -> Result<u64> {
    let line = lines
        .iter()
        .find_map(|line| line.split_once(MIRRORING).map(|(_, rest)| rest))
        .ok_or_else(|| Error::new(format!("boot 1's kernel never said `{MIRRORING}`")))?;
    let hex = line
        .split(',')
        .next()
        .and_then(|word| word.strip_prefix("0x"))
        .ok_or_else(|| Error::new(format!("`{line}` names no address")))?;
    u64::from_str_radix(hex, 16).map_err(|error| Error::new(format!("`{line}`: {error}")))
}

/// Everything the record must show; the first that fails is said.
fn judge(arch: Arch, boot_1: &[String], region: &[u8], after: &[String]) -> Result<()> {
    let fail = |problem: String| Err(Error::new(format!("{arch}: {problem}")));
    // 1. A cold first boot, and a mirror there.
    if !boot_1
        .iter()
        .any(|line| line.contains("no previous kernel log (nothing there)"))
    {
        return fail("boot 1's loader did not find RAM with nothing there".to_owned());
    }
    let address = region_address(boot_1)?;
    let mirror = format!("{MIRRORING}{address:#x}, 1024 KiB, boot 1;");
    if !boot_1.iter().any(|line| line.contains(&mirror)) {
        return fail(format!("boot 1's kernel did not say `{mirror}`"));
    }
    // The default: the report halts, and says so.
    if let Some(line) = boot_1.iter().find(|line| line.contains(RESET_LINE)) {
        return fail(format!(
            "a panic without ferrix.panic=reset announced a reset: `{}`",
            line.trim()
        ));
    }
    if !boot_1.iter().any(|line| line.contains(STOPPED_HALTS)) {
        return fail(format!(
            "boot 1's report, without ferrix.panic=reset, did not say `{}`",
            STOPPED_HALTS.trim()
        ));
    }
    // 2. The region as boot 1 left it, written back once the report was.
    if let Some(problem) = write_back_problem(boot_1).or_else(|| region_problem(boot_1, region)) {
        return fail(problem);
    }
    // 3. The loader again, and what it says of boot 1.
    if !after.iter().any(|line| line.contains(LOADER_BANNER)) {
        return fail("the loader did not start again after the reset".to_owned());
    }
    let found = format!("pstore   at {address:#x}, 1 MiB: boot 1's kernel log, ");
    if !after.iter().any(|line| line.contains(&found)) {
        return fail(format!("boot 2's loader did not say `{found}...`"));
    }
    let panic = boot_1
        .iter()
        .find_map(|line| line.trim_end().strip_prefix("FERRIX-PANIC "))
        .ok_or_else(|| Error::new(format!("{arch}: boot 1 printed no FERRIX-PANIC line")))?;
    let quoted = format!("boot 1{LAST_REPORT}{}", rendered(panic));
    if !after.iter().any(|line| line.trim_end().ends_with(&quoted)) {
        return fail(format!(
            "boot 2's loader did not quote boot 1's panic: wanted a line ending `{quoted}`"
        ));
    }
    if !after
        .iter()
        .any(|line| line.contains(&format!("boot 2{GOES_TO}{address:#x}")))
    {
        return fail("boot 2's loader did not hand boot 2 the same region".to_owned());
    }
    if !after
        .iter()
        .any(|line| line.contains(&format!("{MIRRORING}{address:#x}, 1024 KiB, boot 2;")))
    {
        return fail("boot 2's kernel did not mirror boot 2 to the region".to_owned());
    }
    if let Some(problem) = panic_word_problem(region, after) {
        return fail(problem);
    }
    println!("  {arch}: boot 2's loader found boot 1's log and its panic: {panic}");
    Ok(())
}

/// Why the record does not say boot 1 ended in a panic, if it does not: the
/// saved region's stop word, and boot 2's loader's line.
///
/// Verifies: `L.console.46`
fn panic_word_problem(region: &[u8], after: &[String]) -> Option<String> {
    let header: &[u8; HEADER_BYTES] = region.get(..HEADER_BYTES)?.try_into().ok()?;
    let found = pstore::validate(header, region.len() as u64).ok()?;
    if found.ended() != Ended::Stopped(Stop::Panic) {
        return Some(format!(
            "the region boot 1 left says it ended in {}, not a panic",
            found.ended()
        ));
    }
    let ended = format!("pstore   boot 1 ended: {}", Stop::Panic.says());
    if !after.iter().any(|line| line.trim_end().ends_with(&ended)) {
        return Some(format!("boot 2's loader did not say `{ended}`"));
    }
    None
}

/// Why boot 1's report did not write the region back after its last logged
/// line and before the screen, if it did not: the unlogged `written back`
/// line must follow the panic line and the report's `see` line, which is its
/// last logged one, with nothing but that line's wrapped rest between (the region's own match with
/// the serial log, [`region_problem`], then says the region holds them).
///
/// Verifies: `L.console.44`
fn write_back_problem(boot_1: &[String]) -> Option<String> {
    let panic = boot_1
        .iter()
        .position(|line| line.starts_with("FERRIX-PANIC "))?;
    let report = boot_1.get(panic..)?;
    let Some(back) = report.iter().position(|line| line.contains(WRITTEN_BACK)) else {
        return Some(format!("boot 1's report never said `{WRITTEN_BACK}`"));
    };
    let Some(see) = report
        .iter()
        .rposition(|line| line.starts_with("  see       "))
    else {
        return Some("boot 1's report has no `see` line to end its explanation".to_owned());
    };
    // The `see` text may wrap onto lines indented under it.
    let between = report.get(see.saturating_add(1)..back).unwrap_or_default();
    if back <= see || !between.iter().all(|line| line.starts_with("            ")) {
        return Some(format!(
            "boot 1's report wrote the region back at its line {back}, not straight after its \
             explanation's `see` line {see}"
        ));
    }
    None
}

/// A line as the loader quotes it: printable, cut, `FERRIX-` defused.
fn rendered(line: &str) -> String {
    let mut bytes: Vec<u8> = line.bytes().take(SHOWN).map(pstore::shown).collect();
    pstore::defuse(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Why the saved region is not boot 1's log, if it is not
/// (`L.console.42`): its text must be boot 1's serial log from the kernel's
/// first line, in order, with only the unlogged lines missing, and none of
/// them present.
/// Verifies: `L.console.42`
fn region_problem(boot_1: &[String], region: &[u8]) -> Option<String> {
    let header: &[u8; HEADER_BYTES] = region.get(..HEADER_BYTES)?.try_into().ok()?;
    let found = match pstore::validate(header, region.len() as u64) {
        Ok(found) => found,
        Err(why) => return Some(format!("the saved region's header is refused: {why}")),
    };
    if found.sequence != 1 {
        return Some(format!(
            "the saved region is boot {}'s, not boot 1's",
            found.sequence
        ));
    }
    let ring = region.get(pstore::HEADER as usize..)?;
    let text = Text::of(ring, &found);
    if found.written > found.kept() {
        return Some(format!(
            "boot 1 recorded {} bytes, more than the region keeps: the gate's boot is too long \
             to compare whole",
            found.written
        ));
    }
    let (first, second) = text.parts();
    let mut bytes = first.to_vec();
    bytes.extend_from_slice(second);
    let text = String::from_utf8_lossy(&bytes);
    let recorded: Vec<&str> = text
        .split('\n')
        .map(|line| line.trim_end_matches('\r'))
        .collect();
    // The last piece is what follows the last newline: nothing, after a
    // whole line.
    let (last, whole) = recorded.split_last()?;
    if !last.is_empty() {
        return Some(format!("the region ends inside a line: `{last}`"));
    }
    if let Some(line) = whole.iter().find(|line| unlogged(line)) {
        return Some(format!("the region holds an unlogged line: `{line}`"));
    }
    // The kernel's first logged line is the empty one before its banner, so
    // the region starts there (or at the banner, should the serial reader
    // have joined the empty line to the loader's last): the log so far,
    // copied in when the region was attached.
    let banner = boot_1
        .iter()
        .position(|line| line.starts_with("Ferrix ") && !line.starts_with(LOADER_BANNER))?;
    let mut problem = Some(
        "the region does not start with the kernel's first lines, its banner and the empty line \
         before it"
            .to_owned(),
    );
    for start in (banner.saturating_sub(1)..=banner)
        .filter(|&at| boot_1.get(at).map(String::as_str) == whole.first().copied())
    {
        match matches_serial(whole, boot_1.get(start..).unwrap_or_default()) {
            None => {
                problem = None;
                break;
            }
            Some(why) => problem = Some(why),
        }
    }
    if let Some(why) = problem {
        return Some(why);
    }
    println!(
        "  region   boot 1's {} bytes, {} lines, match its serial log less the unlogged lines",
        found.written,
        whole.len()
    );
    None
}

/// Where `serial` goes on after `line`, if `line` is `sent` and the serial
/// lines from `from` run together: a record cuts an open line on the port,
/// not in the log (722824a8d).
fn joined_through(line: &str, sent: &str, serial: &[String], from: usize) -> Option<usize> {
    if sent.is_empty() || !line.starts_with(sent) {
        return None;
    }
    let mut joined = String::from(sent);
    let mut ahead = from;
    while joined.len() < line.len() {
        joined.push_str(serial.get(ahead)?.trim_end_matches('\r'));
        ahead += 1;
    }
    (joined == line).then_some(ahead)
}

/// Why `recorded`, the region's whole lines, is not `serial` from its start
/// in order with only unlogged lines skipped, if it is not.
fn matches_serial(recorded: &[&str], serial: &[String]) -> Option<String> {
    let mut next = 0;
    for (number, line) in recorded.iter().enumerate() {
        loop {
            let Some(sent) = serial.get(next) else {
                return Some(format!(
                    "the region's line {} `{line}` is not in boot 1's serial log, in order",
                    number + 1
                ));
            };
            next += 1;
            let sent = sent.trim_end_matches('\r');
            if sent == *line {
                break;
            }
            // A record cuts an open line on the port, not in the log
            // (722824a8d): the region's line is that serial line and the
            // ones after it, run together.
            if let Some(after) = joined_through(line, sent, serial, next) {
                next = after;
                break;
            }
            if !unlogged(sent) {
                return Some(format!(
                    "boot 1's serial line `{sent}` is missing from the region before its line {} \
                     `{line}`",
                    number + 1
                ));
            }
        }
    }
    // And the region goes on to the report's end: up to the write-back,
    // whatever the serial log has after the region's last line is unlogged.
    for sent in serial.get(next..).unwrap_or_default() {
        if sent.contains(WRITTEN_BACK) {
            return None;
        }
        if !unlogged(sent) {
            return Some(format!(
                "boot 1's serial line `{}` is missing from the region's end",
                sent.trim_end()
            ));
        }
    }
    None
}

/// `ferrix.panic=reset`: boot 1 panics and the machine resets itself after
/// the wait; boot 2's loader quotes boot 1's panic.
///
/// Verifies: H.FAIL.5, `L.panic.2`, `L.panic.3`
fn reset_by_itself(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut wanted = args.clone();
    wanted.kernel_options.push("ferrix.onexit=panic".to_owned());
    wanted.kernel_options.push("ferrix.panic=reset".to_owned());
    wanted.keep_ram = true;
    let (image, kernel) = build(arch, &wanted)?;
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let mut waited = None;
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let announced = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(RESET_LINE))
        })?;
        if !announced {
            return Err(Error::new(format!(
                "{arch}: boot 1's report under ferrix.panic=reset never said `{}`",
                RESET_LINE.trim()
            )));
        }
        let at = Instant::now();
        let seen = watching.after().len();
        let reset = watching.read_more(at + RESET_WAIT_AT_MOST, |lines| {
            lines
                .get(seen..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(LOADER_BANNER))
        })?;
        if !reset {
            return Err(Error::new(format!(
                "{arch}: the machine did not reset by itself within {} s of `{}`",
                RESET_WAIT_AT_MOST.as_secs(),
                RESET_LINE.trim()
            )));
        }
        waited = Some(at.elapsed());
        let banner = watching
            .after()
            .iter()
            .skip(seen)
            .position(|line| line.contains(LOADER_BANNER))
            .map_or(seen, |at| seen + at);
        boot_1 = watching
            .lines()
            .iter()
            .chain(watching.after().get(..banner).unwrap_or_default())
            .cloned()
            .collect();
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines
                .get(banner..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(MIRRORING))
        })?;
        after = watching.after().get(banner..).unwrap_or_default().to_vec();
        watching.stop_when_done();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &image, &kernel, &wanted, qemu::PANIC_MARKER, hook)?;
    reset_problem(&boot_1, &after, waited).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: {problem}")))
    })?;
    println!(
        "  {arch}: ferrix.panic=reset reset the machine by itself, and the next loader quoted the panic"
    );
    Ok(())
}

/// Why a `ferrix.panic=reset` boot did not reset as it should, if it did not:
/// the report's `stopped` line says so, the reset came after the wait and
/// not much later, and the next loader quotes boot 1's panic.
fn reset_problem(boot_1: &[String], after: &[String], waited: Option<Duration>) -> Option<String> {
    if !boot_1.iter().any(|line| line.contains(STOPPED_RESETS)) {
        return Some(format!(
            "boot 1's report did not say `{}`",
            STOPPED_RESETS.trim()
        ));
    }
    let waited = waited?;
    if waited < RESET_WAIT_AT_LEAST {
        return Some(format!(
            "reset sooner than 10 s: the loader's banner came {} ms after `{}`",
            waited.as_millis(),
            RESET_LINE.trim()
        ));
    }
    let panic = boot_1
        .iter()
        .find_map(|line| line.trim_end().strip_prefix("FERRIX-PANIC "));
    let Some(panic) = panic else {
        return Some("boot 1 printed no FERRIX-PANIC line".to_owned());
    };
    let quoted = format!("boot 1{LAST_REPORT}{}", rendered(panic));
    if !after.iter().any(|line| line.trim_end().ends_with(&quoted)) {
        return Some(format!(
            "after the reset, the loader did not quote boot 1's panic: wanted a line ending `{quoted}`"
        ));
    }
    None
}

/// How a stop case's boot 1 is made to stop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum How {
    /// It resets by itself.
    ByItself,
    /// It powers off or halts, which pauses QEMU; the gate resets it.
    Paused,
    /// Nothing stops it: the gate resets it once it has said it waits.
    Reset,
}

/// One stop case: its name, the script's body (none for the image
/// `test-boot` boots), the option beside it, how boot 1 stops, the line boot
/// 1 says last before it does, and what boot 2's loader must say of it.
#[derive(Debug)]
struct Case {
    name: &'static str,
    script: Option<&'static str>,
    option: Option<&'static str>,
    how: How,
    stopping: &'static str,
    ended: Ended,
}

/// The script's path, which `ferrix.init=` names.
const STOP_SCRIPT: &str = "/etc/pm-stop";
/// What the hang case's script says before it sleeps.
const WAITING: &str = "pm-stop: waiting for the reset";
/// Where the RESTART2 program goes: busybox has the name `reboot`.
const REBOOT2: &str = "bin/reboot2";

/// Every stop case.
const CASES: [Case; 7] = [
    Case {
        name: "reboot(2) RESTART",
        script: Some("reboot -f\n"),
        option: None,
        how: How::ByItself,
        stopping: "reboot: Restarting system",
        ended: Ended::Stopped(Stop::Restart),
    },
    Case {
        name: "reboot(2) RESTART2",
        script: Some("/bin/reboot2 pm-word\n"),
        option: None,
        how: How::ByItself,
        stopping: "reboot: Restarting system with command 'pm-word'",
        ended: Ended::Stopped(Stop::Restart),
    },
    Case {
        name: "reboot(2) POWER_OFF",
        script: Some("poweroff -f\n"),
        option: None,
        how: How::Paused,
        stopping: "reboot: Power down",
        ended: Ended::Stopped(Stop::PowerOff),
    },
    Case {
        name: "reboot(2) HALT",
        script: Some("halt -f\n"),
        option: None,
        how: How::Paused,
        stopping: "reboot: System halted",
        ended: Ended::Stopped(Stop::Halt),
    },
    Case {
        name: "init's exit under ferrix.onexit=reset",
        script: None,
        option: Some("ferrix.onexit=reset"),
        how: How::ByItself,
        stopping: "  power    resetting, as ferrix.onexit=reset asks",
        ended: Ended::Stopped(Stop::ExitReset),
    },
    Case {
        name: "init's exit, powering off",
        script: None,
        option: None,
        how: How::Paused,
        stopping: "FERRIX-POWER-OFF",
        ended: Ended::Stopped(Stop::ExitPowerOff),
    },
    Case {
        name: "a reset from outside",
        script: Some("echo \"pm-stop: waiting for the reset\"\nsleep 600\n"),
        option: None,
        how: How::Reset,
        stopping: WAITING,
        ended: Ended::NoneRecorded,
    },
];

/// The stops: each case's boot 1 stops as it says, and boot 2's loader says
/// how it ended.
///
/// Verifies: `L.console.46`
/// Verifies: `L.console.47`
fn stops(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let busybox = crate::busybox::program(arch)?;
    let shell = read(&busybox)?;
    let reboot2 = read(&crate::compositor::reboot_program(arch)?)?;
    let loader = crate::cargo::build_loader(arch, args.release)?;
    let kernel = crate::cargo::build_kernel_with_init(arch, args.release, &busybox, "")?;
    let natives = crate::native::build(arch, args.release)?;
    let mut carried = crate::shell::carried_for(arch, &busybox, args)?;
    carried.push(crate::ports::File {
        path: REBOOT2.to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(reboot2),
    });
    let parts = crate::init_file::Parts {
        arch,
        loader: &loader,
        kernel: &kernel,
        natives: &natives,
        program: &busybox,
        carried: &carried,
    };
    for case in &CASES {
        let mut wanted = args.clone();
        wanted.keep_ram = true;
        let port = crate::display::free_port()?;
        wanted.qmp_port = Some(port);
        let (image, elf) = if let Some(body) = case.script {
            let mut options = qemu::init_option(STOP_SCRIPT);
            if let Some(option) = case.option {
                options.push(' ');
                options.push_str(option);
            }
            let image = parts.image_with(
                &shell,
                STOP_SCRIPT,
                &format!("#!/bin/sh\n{body}"),
                &options,
                &["bin/reboot", "bin/poweroff", "bin/halt", "bin/sleep"],
            )?;
            (image, kernel.elf.clone())
        } else {
            wanted.kernel_options.extend(case.option.map(str::to_owned));
            build(arch, &wanted)?
        };
        println!("  {arch}: stopping boot 1 by {}", case.name);
        let (boot_1, after) = stopped(arch, &image, &elf, &wanted, port, case)?;
        stop_problem(case, &boot_1, &after).map_or(Ok(()), |problem| {
            Err(Error::new(format!("{arch}: {}: {problem}", case.name)))
        })?;
        println!(
            "  {arch}: {}: boot 2's loader said boot 1 ended in {}",
            case.name, case.ended
        );
    }
    Ok(())
}

/// The bytes of the file at `path`.
fn read(path: &std::path::Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// Boot `image`, stop boot 1 as `case` says, and read until boot 2's kernel
/// attaches: boot 1's lines and boot 2's.
fn stopped(
    arch: Arch,
    image: &std::path::Path,
    elf: &std::path::Path,
    wanted: &Args,
    port: u16,
    case: &Case,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut boot_1 = Vec::new();
    let mut after = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let before = watching
            .lines()
            .iter()
            .any(|line| line.contains(case.stopping));
        let said = before
            || watching.read_more(deadline, |lines| {
                lines.iter().any(|line| line.contains(case.stopping))
            })?;
        if !said {
            return Err(Error::new(format!(
                "{arch}: {}: boot 1 never said `{}`",
                case.name,
                case.stopping.trim()
            )));
        }
        let seen = watching.after().len();
        match case.how {
            How::ByItself => {}
            How::Paused => {
                let mut qmp = Qmp::connect(port, deadline)?;
                if !paused(&mut qmp, deadline)? {
                    return Err(Error::new(format!(
                        "{arch}: {}: QEMU did not pause at boot 1's stop",
                        case.name
                    )));
                }
                let _ = qmp.execute("system_reset", None)?;
                // A paused machine stays paused across the reset; one that
                // runs again already refuses this, which changes nothing.
                let _ = qmp.execute("cont", None);
            }
            How::Reset => {
                let mut qmp = Qmp::connect(port, deadline)?;
                let _ = qmp.execute("system_reset", None)?;
            }
        }
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            let new = lines.get(seen..).unwrap_or_default();
            new.iter()
                .position(|line| line.contains(LOADER_BANNER))
                .is_some_and(|banner| {
                    new.get(banner..)
                        .unwrap_or_default()
                        .iter()
                        .any(|line| line.contains(MIRRORING))
                })
        })?;
        let banner = watching
            .after()
            .iter()
            .skip(seen)
            .position(|line| line.contains(LOADER_BANNER))
            .map_or(watching.after().len(), |at| seen + at);
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
    let _ = qemu::watch_then(arch, image, elf, wanted, qemu::SUCCESS_MARKER, hook)?;
    Ok((boot_1, after))
}

/// Whether QEMU says the machine is paused, asked until `deadline`: a
/// power-off under `-action shutdown=pause`, not an exit.
pub(crate) fn paused(qmp: &mut Qmp, deadline: Instant) -> Result<bool> {
    loop {
        let status = qmp.execute("query-status", None)?;
        if status.replace(' ', "").contains("\"running\":false") {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Why a stop case's boots do not show its stop, if they do not: boot 1
/// said its stopping line on a cold start with a mirror, the loader started
/// again, it says exactly how boot 1 ended, and boot 2's kernel attached the
/// fresh header it gave.
fn stop_problem(case: &Case, boot_1: &[String], after: &[String]) -> Option<String> {
    if !boot_1.iter().any(|line| line.contains(case.stopping)) {
        return Some(format!("boot 1 never said `{}`", case.stopping.trim()));
    }
    if !boot_1
        .iter()
        .any(|line| line.contains("no previous kernel log (nothing there)"))
    {
        return Some("boot 1's loader did not find RAM with nothing there".to_owned());
    }
    let address = match region_address(boot_1) {
        Ok(address) => address,
        Err(error) => return Some(error.to_string()),
    };
    if !after.iter().any(|line| line.contains(LOADER_BANNER)) {
        return Some("the loader did not start again after boot 1's stop".to_owned());
    }
    let ended = format!("pstore   boot 1 ended: {}", case.ended);
    if !after.iter().any(|line| line.trim_end().ends_with(&ended)) {
        let said = after
            .iter()
            .find(|line| line.contains("pstore   boot 1 ended: "))
            .map_or("nothing of it", |line| line.trim());
        return Some(format!(
            "boot 2's loader did not say `{ended}`; it said {said}"
        ));
    }
    let mirror = format!("{MIRRORING}{address:#x}, 1024 KiB, boot 2;");
    if !after.iter().any(|line| line.contains(&mirror)) {
        return Some(format!("boot 2's kernel did not say `{mirror}`"));
    }
    None
}

/// The loader's event-log lines.
const KEPT: &str = "  events   boot 1's panic kept as record #0 (slot 0)";
const EVENTS: &str = "  events   ";
/// Where the log goes on the test disk: as on the N150 box.
const EVENTS_RANGE: (u64, u64) = (40, 2047);
/// Where the ESP starts.
const ESP_FIRST: u64 = 2048;

/// The event log across a power cycle (E12).
fn kept_across_power(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut wanted = args.clone();
    wanted.kernel_options.push("ferrix.onexit=panic".to_owned());
    wanted.kernel_options.push("ferrix.panic=reset".to_owned());
    wanted.keep_ram = true;
    let (image, kernel) = build(arch, &wanted)?;
    let disk = paths::build_dir(arch).join("eventlog-disk.img");
    write_disk(&image, &disk)?;
    let mut boot_1 = Vec::new();
    let mut boot_2 = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE + RESET_WAIT_AT_MOST;
        let seen = watching.after().len();
        let kept = watching.read_more(deadline, |lines| {
            lines
                .get(seen..)
                .unwrap_or_default()
                .iter()
                .any(|line| line.contains(MIRRORING))
                && lines.iter().any(|line| line.contains(LOADER_BANNER))
        })?;
        boot_1 = watching.lines().to_vec();
        boot_2 = watching.after().to_vec();
        if !kept {
            return Err(Error::new(format!(
                "{arch}: boot 2 never started after boot 1's panic reset the machine"
            )));
        }
        // The power cut: the process ends, and RAM with it.
        watching.cut_power();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &disk, &kernel, &wanted, qemu::PANIC_MARKER, hook)?;
    let panic = boot_1
        .iter()
        .find_map(|line| line.trim_end().strip_prefix("FERRIX-PANIC "))
        .ok_or_else(|| Error::new(format!("{arch}: boot 1 printed no FERRIX-PANIC line")))?
        .to_owned();
    if !boot_2.iter().any(|line| line.contains(KEPT)) {
        return Err(Error::new(format!(
            "{arch}: boot 2's loader did not say `{}`",
            KEPT.trim()
        )));
    }
    let mut boot_3 = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(MIRRORING))
        })?;
        boot_3 = watching
            .lines()
            .iter()
            .chain(watching.after())
            .cloned()
            .collect();
        watching.cut_power();
        Ok(())
    };
    let _ = qemu::watch_then(arch, &disk, &kernel, &wanted, LOADER_BANNER, hook)?;
    let record = read_log(&disk)?;
    kept_problem(&panic, &boot_3, &record).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: {problem}")))
    })?;
    println!("  {arch}: boot 1's panic outlived a power cycle in the event log: {panic}");
    Ok(())
}

/// The newest record on the disk's event log: its count, its first line
/// and its text, or why there is none.
fn read_log(disk: &std::path::Path) -> Result<Option<(u32, String)>> {
    use ferrix_eventlog::{self as eventlog, Geometry, RECORD_LEN};
    let file = std::fs::File::open(disk)?;
    let mut device = LogDisk(&file);
    let part = ferrix_slots::gpt::Part {
        index: 0,
        unique: [0; 16],
        first_lba: EVENTS_RANGE.0,
        last_lba: EVENTS_RANGE.1,
    };
    let geometry =
        Geometry::of(&part, 512).map_err(|why| Error::new(format!("the log: {why:?}")))?;
    let eventlog::Current::Header(header) = eventlog::read_header(&mut device, &geometry)
        .map_err(|why| Error::new(format!("the log's header: {why:?}")))?
    else {
        return Ok(None);
    };
    let Some(slot) = header.newest_slot(geometry.records()) else {
        return Ok(None);
    };
    let mut buffer = vec![0u8; RECORD_LEN];
    let meta = eventlog::read_record(&mut device, &geometry, slot, &mut buffer)
        .map_err(|why| Error::new(format!("the log's record: {why:?}")))?;
    let Some(meta) = meta.filter(|meta| eventlog::counted(&header, meta)) else {
        return Ok(None);
    };
    let text = eventlog::decode_record(&buffer)
        .map(|(_, text)| String::from_utf8_lossy(text).into_owned())
        .unwrap_or_default();
    Ok(Some((meta.count, text)))
}

/// Why the record did not outlive the power cycle as it should, if it did
/// not: on the disk, its text boot 1's report as the loader renders it, and
/// boot 3's loader, which found no pstore, quoting it.
fn kept_problem(panic: &str, boot_3: &[String], record: &Option<(u32, String)>) -> Option<String> {
    let Some((_, text)) = record else {
        return Some("the event log on the disk holds no record after the power cycle".to_owned());
    };
    let first = format!("FERRIX_PANIC {}", rendered(panic));
    if !text.starts_with(&first) {
        return Some(format!(
            "the event log's record does not start with boot 1's panic: wanted `{first}`"
        ));
    }
    if !boot_3
        .iter()
        .any(|line| line.contains("no previous kernel log (nothing there)"))
    {
        return Some("boot 3's loader found a pstore after the power cycle".to_owned());
    }
    let summary = format!("newest #0: boot 1 panicked, 1 time(s): {first}");
    if !boot_3
        .iter()
        .any(|line| line.contains(EVENTS) && line.trim_end().ends_with(&summary))
    {
        return Some(format!(
            "boot 3's loader did not quote the record: wanted a line ending `{summary}`"
        ));
    }
    None
}

/// Write `disk`: a GUID partition table, the event log at
/// [`EVENTS_RANGE`] (zeroed: no header yet), and `image`, the ESP, from
/// [`ESP_FIRST`].
pub(crate) fn write_disk(image: &std::path::Path, disk: &std::path::Path) -> Result<()> {
    use ferrix_partition::{ESP, Guid, Partition, write_table};
    let esp = std::fs::read(image)?;
    let esp_sectors = (esp.len() as u64).div_ceil(512);
    let sectors = ESP_FIRST + esp_sectors + 2048;
    let partitions = [
        Partition::new(
            Guid(ferrix_eventlog::EVENTS_TYPE),
            Guid::random([0x31; 16]),
            EVENTS_RANGE.0,
            EVENTS_RANGE.1,
            "ferrix-events",
        ),
        Partition::new(
            ESP,
            Guid::random([0x32; 16]),
            ESP_FIRST,
            ESP_FIRST + esp_sectors - 1,
            "ferrix-esp",
        ),
    ];
    let table = write_table(sectors, 512, Guid::random([0x33; 16]), &partitions)
        .map_err(|why| Error::new(format!("the event-log disk's table: {why:?}")))?;
    let mut bytes = vec![0u8; usize::try_from(sectors * 512).unwrap_or(0)];
    for (lba, block) in table {
        let at = usize::try_from(lba * 512).unwrap_or(usize::MAX);
        if let Some(place) = bytes.get_mut(at..at.saturating_add(block.len())) {
            place.copy_from_slice(&block);
        }
    }
    let at = usize::try_from(ESP_FIRST * 512).unwrap_or(usize::MAX);
    if let Some(place) = bytes.get_mut(at..at.saturating_add(esp.len())) {
        place.copy_from_slice(&esp);
    }
    std::fs::write(disk, bytes)?;
    Ok(())
}

/// The event log of a disk [`write_disk`] wrote, as a device and its
/// geometry: for reading it back, and for the host's clear.
pub(crate) fn log_device(file: &std::fs::File) -> Result<(LogDisk<'_>, ferrix_eventlog::Geometry)> {
    let part = ferrix_slots::gpt::Part {
        index: 0,
        unique: [0; 16],
        first_lba: EVENTS_RANGE.0,
        last_lba: EVENTS_RANGE.1,
    };
    let geometry = ferrix_eventlog::Geometry::of(&part, 512)
        .map_err(|why| Error::new(format!("the log: {why:?}")))?;
    Ok((LogDisk(file), geometry))
}

/// The disk file as the event log's device: read back, and written by the
/// host's clear (the file must then be open for writing).
pub(crate) struct LogDisk<'a>(&'a std::fs::File);

impl ferrix_slots::record::ControlDisk for LogDisk<'_> {
    fn block_size(&self) -> u32 {
        512
    }
    fn write_block(
        &mut self,
        lba: u64,
        bytes: &[u8],
    ) -> std::result::Result<(), ferrix_slots::record::DeviceError> {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = self.0;
        file.seek(SeekFrom::Start(lba * 512))
            .and_then(|_| file.write_all(bytes))
            .map_err(|_| ferrix_slots::record::DeviceError)
    }
    fn flush(&mut self) -> std::result::Result<(), ferrix_slots::record::DeviceError> {
        self.0
            .sync_data()
            .map_err(|_| ferrix_slots::record::DeviceError)
    }
    fn read_block(
        &mut self,
        lba: u64,
        bytes: &mut [u8],
    ) -> std::result::Result<(), ferrix_slots::record::DeviceError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = self.0;
        file.seek(SeekFrom::Start(lba * 512))
            .and_then(|_| file.read_exact(bytes))
            .map_err(|_| ferrix_slots::record::DeviceError)
    }
}

/// What the kernel says of a value of `ferrix.panic` it does not take.
const MISSPELT: &str = "  panic    ferrix.panic=Reset is not understood; a panic halts";

/// `ferrix.panic=Reset`: said as not understood, and the panic halts.
///
/// Verifies: `L.panic.1`
fn misspelt(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut wanted = args.clone();
    wanted.kernel_options.push("ferrix.onexit=panic".to_owned());
    wanted.kernel_options.push("ferrix.panic=Reset".to_owned());
    let (image, kernel) = build(arch, &wanted)?;
    let mut lines = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        let deadline = Instant::now() + PATIENCE;
        let _ = watching.read_more(deadline, |lines| {
            lines.iter().any(|line| line.contains(WRITTEN_BACK))
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
    let _ = qemu::watch_then(arch, &image, &kernel, &wanted, qemu::PANIC_MARKER, hook)?;
    misspelt_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: {problem}")))
    })?;
    println!("  {arch}: ferrix.panic=Reset was not understood, and the panic halted");
    Ok(())
}

/// Why a `ferrix.panic=Reset` boot did not take the value as halt, if it
/// did not.
fn misspelt_problem(lines: &[String]) -> Option<String> {
    if !lines.iter().any(|line| line.contains(MISSPELT)) {
        return Some(format!("the kernel did not say `{}`", MISSPELT.trim()));
    }
    if let Some(line) = lines.iter().find(|line| line.contains(RESET_LINE)) {
        return Some(format!(
            "a misspelt ferrix.panic=Reset announced a reset: `{}`",
            line.trim()
        ));
    }
    if !lines.iter().any(|line| line.contains(STOPPED_HALTS)) {
        return Some(format!(
            "the report under ferrix.panic=Reset did not say `{}`",
            STOPPED_HALTS.trim()
        ));
    }
    None
}

/// The switch: `ferrix.pstore=off` takes no region, on both sides.
fn switched_off(
    arch: Arch,
    args: &Args,
    build: &impl Fn(Arch, &Args) -> Result<(PathBuf, PathBuf)>,
) -> Result<()> {
    let mut off = args.clone();
    off.kernel_options.push("ferrix.pstore=off".to_owned());
    let (image, kernel) = build(arch, &off)?;
    let lines = qemu::watch_lines(arch, &image, &kernel, &off, qemu::SUCCESS_MARKER)?;
    off_problem(&lines).map_or(Ok(()), |problem| {
        Err(Error::new(format!("{arch}: {problem}")))
    })?;
    println!("  {arch}: ferrix.pstore=off took no region");
    Ok(())
}

/// Why a `ferrix.pstore=off` boot kept a log, if it did.
fn off_problem(lines: &[String]) -> Option<String> {
    let said_off = format!("pstore   off (ferrix.pstore=off): {NONE}");
    if !lines.iter().any(|line| line.contains(&said_off)) {
        return Some(format!("the loader did not say `{said_off}`"));
    }
    if let Some(line) = lines
        .iter()
        .find(|line| line.contains(GOES_TO) || line.contains(MIRRORING))
    {
        return Some(format!(
            "a region was taken all the same: `{}`",
            line.trim()
        ));
    }
    let none = "pstore   none (the loader gave no region)";
    if !lines.iter().any(|line| line.contains(none)) {
        return Some(format!("the kernel did not say `{none}`"));
    }
    None
}

#[cfg(test)]
mod tests;
