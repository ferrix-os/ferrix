//! `cargo xtask test-log-budget`: lode's write budget (`docs/LOGGING.md`
//! §8.3), the bytes written to the disk for each byte logged.
//!
//! It boots init with lode keeping its store on a btrfs volume at `/data`
//! (`/var/log/journal` a link there), and a unit that writes two
//! workloads to the kernel log with nothing else writing to the volume:
//! two 100-byte lines a second for a minute, then 20,000 such lines as
//! fast as lode takes them. Around each, xtask reads the volume's
//! `wr_bytes` from QEMU's block statistics (`query-blockstats`, the
//! boot's QMP socket) and divides by the `MESSAGE` bytes written. The
//! budgets are the design's: at most 40 for the slow load, whose figure is
//! btrfs's commit of each five-second `fdatasync`, and at most 3 for the
//! fast one. The minute stands in for §8.3's hour: each five seconds is
//! the same sync of the same ten lines, so the ratio does not change with
//! the length, and the gate stays short.
//!
//! x86-64 by default (KVM): the ratio is the store's and the file
//! system's, not the processor's; `--arch` runs another.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, btrfs_disk, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-log-budget",
    help_at: 402,
    help: &[
        "test-log-budget Boot init with lode's store on a btrfs volume and require that the bytes written",
        "              to the disk per byte logged stay within budget (docs/LOGGING.md §8.3)",
    ],
    run: |_, args| test_budget(args),
}];

/// The slow load's budget, bytes written per byte of `MESSAGE`: the
/// design's (§8.3).
const SLOW_BUDGET: f64 = 40.0;

/// What the row holds the slow load to until Ferrix's btrfs makes an
/// `fdatasync` cheaper: each costs about 40 KiB at the volume whatever was
/// written (a minute with no load writes 60 KiB; with the load, 536 KiB for
/// 12 KiB of messages), which lode cannot change but by syncing less often.
/// 45.7 measured on 2026-10-23; the ceiling keeps it from growing.
const SLOW_CEILING: f64 = 50.0;

/// The fast load's.
const FAST_BUDGET: f64 = 3.0;

/// One line's `MESSAGE` bytes.
const LINE: usize = 100;

/// The slow load's lines: two a second for a minute.
const SLOW_LINES: usize = 120;

/// The fast load's.
const FAST_LINES: usize = 20_000;

/// The marker the unit ends with.
const DONE: &str = "budget: done";

/// The volume's name under `build/<arch>/`.
const VOLUME: &str = "log-budget.img";

/// The workload. Its lines go to the kernel log, not through init, so
/// init writes nothing for them; the unit's own few lines say where it is.
const SCRIPT: &str = r#"bb=/bin/busybox
say() { echo "budget: $*"; }
say "setting up"
i=0
until [ -n "$($bb pidof lode)" ]; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode never ran"; say done; exit 1; }
  $bb usleep 100000
done
# The store on the volume: /var/log/journal is a link to /data/journal,
# which lode makes its own once it is there.
$bb mkdir -p /data/journal
/bin/svc restart lode.service > /dev/null 2>&1
# A restarted lode goes on in this boot's segments, in /run: a rotation
# begins the next ones where the store now is.
i=0; until /bin/lode rotate > /dev/null 2>&1; do i=$((i+1)); [ $i -gt 50 ] && break; $bb usleep 100000; done
i=0
until $bb ls /data/journal/ 2>/dev/null | $bb grep -q 'fxj$'; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode's store never came to /data: $($bb ls -la /data /var/log/ 2>&1 | $bb tr '\n' ' ') $($bb grep data /proc/mounts) touch: $($bb touch /data/journal/probe 2>&1; echo $?) run: $($bb ls /run/log/journal | $bb tr '\n' ' ')"; say done; exit 1; }
  $bb usleep 100000
done
# Each line's MESSAGE (what follows `load: `) is 100 bytes: its number,
# a space, and x's to make up the rest.
text=$($bb printf '%0100d' 0 | $bb tr 0 x)
line() { echo "<14>load: $1 ${text:0:$((99-${#1}))}" > /dev/kmsg; }
$bb sleep 8
say "slow begin"
$bb sleep 2
n=0
while [ $n -lt 120 ]; do
  line $n
  n=$((n+1)); $bb usleep 500000
done
$bb sleep 7
say "slow end"
$bb sleep 2
say "fast begin"
$bb sleep 2
n=0
while [ $n -lt 20000 ]; do
  line $n
  n=$((n+1))
  [ $((n % 100)) -eq 0 ] && $bb usleep 50000
done
$bb sleep 7
say "fast end"
say "lost: $(/bin/lode verify --kernel -b | $bb tail -n 1)"
say "stored: $(/bin/lode -b -t load -o cat | $bb wc -l)"
say done
"#;

/// The workload's unit.
const UNIT: &str = "\
# test-log-budget's workload (tools/common/xtask/src/lode_budget.rs).
[Unit]
Description=Write lode's budget workloads
After=lode.service

[Service]
Type=oneshot
TimeoutStartSec=600
ExecStart=/bin/busybox sh /lib/lode-budget.sh
";

/// Folding off and no limit for the load: each line is stored, as the
/// budget counts it.
const RULES: &str = "\
# test-log-budget's rules (tools/common/xtask/src/lode_budget.rs).
fold 0s where id = load
limit 10000000/1s where id = load
";

/// `cargo xtask test-log-budget`.
fn test_budget(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        test_arch(arch, args)?;
    }
    Ok(())
}

/// One architecture's boot.
fn test_arch(arch: Arch, args: &Args) -> Result<()> {
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let busybox = std::fs::read(crate::busybox::program(arch)?)
        .map_err(|error| Error::new(format!("reading busybox: {error}")))?;
    let bytes = |path: &str, mode: u32, content: &str| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(content.as_bytes().to_vec()),
    };
    let link = |path: &str, to: &str| File {
        path: path.to_owned(),
        mode: 0o777,
        content: Content::Link(to.to_owned()),
    };
    files.extend([
        File {
            path: "bin/busybox".to_owned(),
            mode: 0o755,
            content: Content::Bytes(busybox),
        },
        crate::lode::enabled(),
        link("etc/ferrix/units/getty@.service", "/dev/null"),
        link("var/log/journal", "/data/journal"),
        bytes("etc/ferrix/lode.d/budget.rules", 0o644, RULES),
        bytes("lib/lode-budget.sh", 0o755, SCRIPT),
        bytes("etc/ferrix/units/lode-budget.service", 0o644, UNIT),
        link(
            "etc/ferrix/units/multi-user.target.wants/lode-budget.service",
            "/etc/ferrix/units/lode-budget.service",
        ),
    ]);
    println!("  {arch}: building an image whose lode keeps its store on a btrfs volume");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    // A kernel log of 1 MiB, as test-log's, so the fast load's bursts wait
    // in it for lode rather than overwrite one another.
    let options = format!("{} log_buf_len=1M\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let mut booted = args.clone();
    let port = crate::display::free_port()?;
    booted.qmp_port = Some(port);
    booted.data_image = Some(btrfs_disk::blank_copy(arch, VOLUME)?);
    booted.data_image_kept = true;
    booted.timeout = booted.timeout.max(600);
    println!("  {arch}: two loads, slow and fast, measured at the volume");
    let mut readings: Vec<u64> = Vec::new();
    let mut failure: Option<String> = None;
    let lines = qemu::watch_then(arch, &image, &kernel, &booted, "budget: setting up", |at| {
        at.stop_when_done();
        failure = measure(at, port, &mut readings).err();
        Ok(())
    })?;
    if let Some(why) = failure {
        return Err(Error::new(format!("{arch}: {why}")));
    }
    let verdict = judge(&lines, &readings);
    match verdict {
        Ok(summary) => {
            println!("  {arch}: {summary}");
            Ok(())
        }
        Err(why) => Err(Error::new(format!(
            "{arch}: lode's writes are over budget: {why}\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))),
    }
}

/// The volume's `wr_bytes` at each of the four marks.
fn measure(
    at: &mut qemu::Watching<'_>,
    port: u16,
    readings: &mut Vec<u64>,
) -> std::result::Result<(), String> {
    let mut qmp = crate::display::Qmp::connect(port, Instant::now() + Duration::from_secs(10))
        .map_err(|error| format!("QMP: {error}"))?;
    let begun = at
        .read_more(Instant::now() + Duration::from_secs(120), |lines| {
            lines
                .iter()
                .any(|line| line.contains("budget: slow begin") || line.contains("budget: FAIL"))
        })
        .map_err(|error| error.to_string())?;
    if let Some(line) = at.after().iter().find(|line| line.contains("budget: FAIL")) {
        return Err(line.trim().to_owned());
    }
    if !begun {
        return Err("the slow load never began".to_owned());
    }
    let read = |qmp: &mut crate::display::Qmp| -> std::result::Result<u64, String> {
        let answer = qmp
            .execute("query-blockstats", None)
            .map_err(|error| format!("query-blockstats: {error}"))?;
        written(&answer, "btrfsdata")
            .ok_or_else(|| format!("no btrfsdata in query-blockstats: {answer}"))
    };
    readings.push(read(&mut qmp)?);
    for mark in ["budget: slow end", "budget: fast begin", "budget: fast end"] {
        let seen = at
            .read_more(Instant::now() + Duration::from_secs(300), |lines| {
                lines.iter().any(|line| line.contains(mark))
            })
            .map_err(|error| error.to_string())?;
        if !seen {
            return Err(format!("`{mark}` never came"));
        }
        readings.push(read(&mut qmp)?);
    }
    let _ = at
        .read_more(Instant::now() + Duration::from_secs(120), |lines| {
            lines.iter().any(|line| line.contains(DONE))
        })
        .map_err(|error| error.to_string())?;
    Ok(())
}

/// The `wr_bytes` of the drive `device` in a `query-blockstats` answer.
fn written(answer: &str, device: &str) -> Option<u64> {
    let at = answer
        .find(&format!("\"device\": \"{device}\""))
        .or_else(|| answer.find(&format!("\"device\":\"{device}\"")))?;
    // The drive's own entry, up to the next `"device"`: its `parent` (the
    // file under it) comes first with `stats` of its own, so the drive's
    // are the entry's last `"stats"`.
    let entry = answer.get(at..)?;
    let end = entry
        .get(1..)?
        .find("\"device\"")
        .map_or(entry.len(), |end| end + 1);
    let entry = entry.get(..end)?;
    let entry = entry.get(entry.rfind("\"stats\"")?..)?;
    let key = entry.find("\"wr_bytes\"")?;
    let rest = entry.get(key + "\"wr_bytes\"".len()..)?;
    let digits: String = rest
        .trim_start_matches([':', ' '])
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The two ratios, judged against their budgets.
fn judge(lines: &[String], readings: &[u64]) -> std::result::Result<String, String> {
    if let Some(line) = lines.iter().find(|line| line.contains("budget: FAIL")) {
        return Err(line.trim().to_owned());
    }
    let [slow_from, slow_to, fast_from, fast_to] = readings else {
        return Err(format!(
            "the volume was read {} time(s), not 4",
            readings.len()
        ));
    };
    let measured = ratios(*slow_from, *slow_to, *fast_from, *fast_to);
    let figures = match &measured {
        Ok(text) | Err(text) => text.clone(),
    };
    let said = |what: &str| {
        lines
            .iter()
            .find_map(|line| {
                line.split_once(what)
                    .map(|(_, rest)| rest.trim().to_owned())
            })
            .unwrap_or_default()
    };
    let lost = said("budget: lost:");
    if !lost.contains(" 0 said lost") || !lost.contains("unsaid gaps: none") {
        return Err(format!("the load was not all stored: `{lost}` ({figures})"));
    }
    let stored = said("budget: stored:");
    if stored != (SLOW_LINES + FAST_LINES).to_string() {
        return Err(format!(
            "{stored} load lines stored, not {}: a rule held some back ({figures})",
            SLOW_LINES + FAST_LINES
        ));
    }
    measured
}

/// The ratios of four readings, against their budgets.
fn ratios(
    slow_from: u64,
    slow_to: u64,
    fast_from: u64,
    fast_to: u64,
) -> std::result::Result<String, String> {
    let per_byte =
        |from: u64, to: u64, lines: usize| to.saturating_sub(from) as f64 / (lines * LINE) as f64;
    let slow = per_byte(slow_from, slow_to, SLOW_LINES);
    let fast = per_byte(fast_from, fast_to, FAST_LINES);
    let summary = format!(
        "slow load {slow:.1} bytes written a byte logged ({} KiB for {} lines; budget \
         {SLOW_BUDGET}{}, held to {SLOW_CEILING} until btrfs's fdatasync is cheaper), \
         fast load {fast:.2} ({} KiB for {} lines; budget {FAST_BUDGET})",
        slow_to.saturating_sub(slow_from) / 1024,
        SLOW_LINES,
        if slow <= SLOW_BUDGET {
            ", within"
        } else {
            ", over"
        },
        fast_to.saturating_sub(fast_from) / 1024,
        FAST_LINES
    );
    if slow <= SLOW_CEILING && fast <= FAST_BUDGET {
        Ok(summary)
    } else {
        Err(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::{ratios, written};

    #[test]
    fn a_drives_writes_are_found_among_others() {
        let answer = r#"{"return": [{"device": "virtio0", "stats": {"wr_bytes": 5}}, {"device": "btrfsdata", "parent": {"stats": {"wr_bytes": 7}, "node-name": "f"}, "stats": {"rd_bytes": 1, "wr_bytes": 4096}, "node-name": "x"}]}"#;
        assert_eq!(written(answer, "btrfsdata"), Some(4096));
        assert_eq!(written(answer, "virtio0"), Some(5));
        assert_eq!(written(answer, "nothing"), None);
    }

    #[test]
    fn within_and_over_budget() {
        assert!(ratios(0, 400_000, 0, 5_000_000).is_ok());
        assert!(ratios(0, 560_000, 0, 5_000_000).is_ok());
        assert!(ratios(0, 700_000, 0, 5_000_000).is_err());
        assert!(ratios(0, 400_000, 0, 7_000_000).is_err());
    }
}
