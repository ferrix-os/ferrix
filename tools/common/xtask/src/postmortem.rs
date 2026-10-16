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

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ferrix_bootinfo::pstore::{self, HEADER_BYTES, Text};

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
        "test-postmortem  the kernel's log across a warm reset: a panic, a QMP reset, the next loader quotes it; and ferrix.pstore=off (x86_64, aarch64)",
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
    judge(arch, &boot_1, &region, &after)
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
    println!("  {arch}: boot 2's loader found boot 1's log and its panic: {panic}");
    Ok(())
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
