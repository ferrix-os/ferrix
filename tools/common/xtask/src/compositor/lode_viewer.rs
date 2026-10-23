//! The boot of the log viewer app (`docs/LOGGING.md`, the viewer).
//!
//! The desktop comes up with the viewer open on its Logs page over a JSONL
//! export the image carries: the app's own test records, 801 of the N150's
//! first boot under lode. Keys reach the guest as a person's would: QEMU
//! presses a function key, the key is bound to `/bin/vkbd`, and vkbd types
//! into the window that has the keyboard.
//!
//! What is required, in order:
//!
//! 1. The viewer read the file's 801 records and its window is up, drawn
//!    with the design's fonts -- on the CPU, as every QEMU guest draws.
//! 2. `/`, `id=msr` and Return typed into it run lode's filter language
//!    over the records: the viewer says it found the 13 lines `msr` wrote,
//!    of 801. The number is the file's, counted on the host when the file
//!    was taken; a viewer that did not run the query, or ran it on other
//!    records, says another.
//!
//! The screen after step 2 is kept in the build directory as
//! `lode-viewer.ppm`.

use std::time::{Duration, Instant};

use super::boot::{judge_still_running, judged_image, press, say_the_marker, with_the_transcript};
use super::{Carried, EITHER, MARKER, Programs, SETTLE};
use crate::args::Args;
use crate::display::{DEVICE_ID, Qmp, free_port};
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::Watching;
use crate::{Error, Result};

/// Where the image carries the records.
const RECORDS: &str = "/usr/share/ferrix/lode-viewer-test.jsonl";

/// The app's own test records, in its folder.
const FIXTURE: &str = "testdata/n150-boot-excerpt.jsonl";

/// The boot's `hyprland.conf`: the viewer as the desktop comes up, and the
/// key that types a query into it.
const CONFIG: &str = "\
# Carried into the initramfs by `cargo xtask test-compositor --boot lode-viewer`.
exec-once = /bin/lode-viewer /usr/share/ferrix/lode-viewer-test.jsonl --page logs --zoom 0
bind = , F1, exec, /bin/vkbd slash i d equal m s r Return
";

/// The records the file holds, and how many of them `msr` wrote.
const RECORDS_IN_FILE: usize = 801;
/// The lines `id=msr` finds.
const MSR_LINES: usize = 13;

/// How long one step may take under emulation on a busy host.
const PATIENCE: Duration = Duration::from_secs(150);

/// Everything the boot carries: the viewer as its package installs it, and
/// its test records.
fn carried(arch: Arch) -> Result<Carried> {
    let mut ports = crate::apps::taken(arch, &["lode-viewer"])?;
    let path = crate::apps::folder("lode-viewer")?.join(FIXTURE);
    let bytes =
        std::fs::read(&path).map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    ports.push(File {
        path: RECORDS.trim_start_matches('/').to_owned(),
        mode: 0o644,
        content: Content::Bytes(bytes),
    });
    Ok(Carried {
        ports,
        ..Carried::none()
    })
}

/// Build what the boot carries ahead of the first boot (`build_ahead`).
pub(super) fn build_ahead(arch: Arch) -> Result<()> {
    let _ = crate::apps::program(arch, "lode-viewer", "lode-viewer")?;
    Ok(())
}

/// The viewer's boot.
pub(super) fn test_lode_viewer(arch: Arch, programs: &Programs, args: &Args) -> Result<()> {
    let (image, kernel) = judged_image(arch, programs, CONFIG, carried(arch)?, None, args)?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.display = true;
    qemu_args.qmp_port = Some(port);
    let dump = paths::build_dir(arch).join("lode-viewer.ppm");
    let mut said = Vec::new();
    let hook = |watching: &mut Watching<'_>| -> Result<()> {
        watching.stop_when_done();
        said = drive(arch, port, &dump, watching)?;
        Ok(())
    };
    let _ = crate::qemu::watch_then(arch, &image, &kernel, &qemu_args, EITHER, hook)?;
    judge_still_running(arch, &said)?;
    judge(arch, &said)?;
    println!(
        "  {arch}: the log viewer read {RECORDS_IN_FILE} records, drew its window, and a query \
         typed into it found the {MSR_LINES} lines msr wrote"
    );
    Ok(())
}

/// Wait until a line has `wanted` in it, or fail saying `what` never
/// happened, with the guest's last lines.
fn wait(watching: &mut Watching<'_>, arch: Arch, wanted: &str, what: &str) -> Result<()> {
    let came = watching.read_more(Instant::now() + PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(wanted))
    })?;
    if came {
        return Ok(());
    }
    Err(with_the_transcript(
        &Error::new(format!("{arch}: {what}: the guest never said `{wanted}`")),
        watching,
    ))
}

/// Drive the boot, and answer everything the guest said.
fn drive(
    arch: Arch,
    port: u16,
    dump: &std::path::Path,
    watching: &mut Watching<'_>,
) -> Result<Vec<String>> {
    let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
    let up = watching.read_more(Instant::now() + SETTLE, |lines| {
        lines.iter().any(|line| line.contains(MARKER))
    })?;
    if !up && !watching.lines().iter().any(|line| line.contains(MARKER)) {
        return Err(Error::new(format!(
            "{arch}: the compositor never printed `{MARKER}`"
        )));
    }
    say_the_marker(watching, arch);
    wait(
        watching,
        arch,
        "lode-viewer: the window is up",
        "the log viewer did not open its window",
    )?;
    println!("  {arch}: the log viewer is up");
    // A moment for the window to have the keyboard.
    std::thread::sleep(Duration::from_secs(2));
    press(&mut qmp, &["f1"])?;
    wait(
        watching,
        arch,
        "lode-viewer: where `id=msr`",
        "the query typed into the log viewer was not run",
    )?;
    std::thread::sleep(Duration::from_secs(2));
    qmp.screendump(Some(DEVICE_ID), dump)?;
    println!("  {arch}: the query ran; the screen is {}", dump.display());
    watching.read_what_was_said(Duration::from_secs(2))?;
    Ok(watching
        .lines()
        .iter()
        .chain(watching.after())
        .cloned()
        .collect())
}

/// What the boot requires of its transcript.
fn judge(arch: Arch, said: &[String]) -> Result<()> {
    let has = |wanted: &str| said.iter().any(|line| line.contains(wanted));
    let refuse = |why: String| Err(Error::new(format!("{arch}: {why}")));
    let read = format!("lode-viewer: read {RECORDS_IN_FILE} records from {RECORDS}");
    if !has(&read) {
        return refuse(format!("the log viewer never said `{read}`"));
    }
    if !has("lode-viewer: the window is up") {
        return refuse("the log viewer never drew its window".to_owned());
    }
    let found = format!("lode-viewer: where `id=msr`: {MSR_LINES} of {RECORDS_IN_FILE} records");
    if !has(&found) {
        let said_instead = said
            .iter()
            .find(|line| line.contains("lode-viewer: where "))
            .map_or("nothing", |line| line.trim());
        return refuse(format!(
            "the query typed into the log viewer did not find msr's {MSR_LINES} lines: it said {said_instead}"
        ));
    }
    if let Some(line) = said.iter().find(|line| line.contains("lode-viewer: err: ")) {
        return refuse(format!("the log viewer complained: {}", line.trim()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CONFIG, judge};
    use crate::paths::Arch;

    fn passed() -> Vec<String> {
        [
            "lode-viewer: read 801 records from /usr/share/ferrix/lode-viewer-test.jsonl (0 other lines)",
            "lode-viewer: 801 records in 1 boots, 19 sources",
            "lode-viewer: the window is up, 1264x704 at scale 1, 754 lines in the table, first frame in 80 ms",
            "vkbd: typed slash i d equal m s r Return",
            "lode-viewer: where `id=msr`: 13 of 801 records",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn refusal(said: &[String]) -> String {
        judge(Arch::X86_64, said)
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default()
    }

    #[test]
    fn a_boot_that_drew_and_queried_passes() {
        assert_eq!(refusal(&passed()), "");
    }

    #[test]
    fn a_query_that_found_other_lines_or_none_fails() {
        let wrong: Vec<String> = passed()
            .into_iter()
            .map(|line| line.replace("13 of 801", "801 of 801"))
            .collect();
        assert!(
            refusal(&wrong).contains("did not find msr's 13 lines"),
            "{}",
            refusal(&wrong)
        );
        let none: Vec<String> = passed()
            .into_iter()
            .filter(|line| !line.contains("where "))
            .collect();
        assert!(
            refusal(&none).contains("it said nothing"),
            "{}",
            refusal(&none)
        );
        let unread: Vec<String> = passed()
            .into_iter()
            .filter(|line| !line.contains("read 801"))
            .collect();
        assert!(refusal(&unread).contains("never said"));
        let complained: Vec<String> = passed()
            .into_iter()
            .chain(["lode-viewer: err: no font was found".to_owned()])
            .collect();
        assert!(refusal(&complained).contains("complained"));
    }

    #[test]
    fn the_key_types_the_query_with_no_key_twice() {
        let bind = CONFIG
            .lines()
            .find(|line| line.contains("vkbd"))
            .expect("the bind");
        let keys: Vec<&str> = bind
            .split("vkbd ")
            .nth(1)
            .unwrap_or("")
            .split(' ')
            .collect();
        let mut seen = std::collections::HashSet::new();
        // vkbd releases its keys in reverse, so a key named twice would be
        // pressed while it is held.
        assert!(keys.iter().all(|key| seen.insert(*key)), "{keys:?}");
    }
}
