//! lode, the log manager (`docs/LOGGING.md`): what an image carries for it,
//! and `cargo xtask test-log`.
//!
//! The program is a native one, `/sbin/lode`, in every initramfs
//! (`native::PROGRAMS`); an image that boots init carries `/bin/lode` and
//! `/bin/journalctl` as links to it and `lode.service` among the shipped
//! units, and an image that wants the log kept enables the unit
//! ([`enabled`]): the desktop's, and `test-log`'s.
//!
//! `test-log` boots init with lode enabled and a unit that floods the
//! kernel log with 3,000 numbered lines, kills lode with `SIGKILL` a third
//! of the way through, and lets init start it again. It requires that every
//! kernel sequence number between the first and the last stored is either
//! stored or covered by a record that says it was lost (`lode verify
//! --kernel`), that the restart was recorded, that the flood's last line is
//! stored, and that the unit's own lines are stored under its unit's name.

use crate::args::Args;
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-log",
    help_at: 401,
    help: &[
        "test-log      Boot init with lode, flood the kernel log, kill lode mid-flood, and require that",
        "              no record is lost but the ones a record says were",
    ],
    run: |_, args| test_log(args),
}];

/// The service's unit, shipped in `/lib/ferrix/units` with init's.
pub(crate) const UNIT: &str = "lode.service";

/// The links an image that boots init carries to the program.
pub(crate) fn links() -> Vec<File> {
    ["bin/lode", "bin/journalctl"]
        .into_iter()
        .map(|path| File {
            path: path.to_owned(),
            mode: 0o777,
            content: Content::Link("/sbin/lode".to_owned()),
        })
        .collect()
}

/// The link that starts `lode.service` with `basic.target`.
pub(crate) fn enabled() -> File {
    File {
        path: format!("etc/ferrix/units/basic.target.wants/{UNIT}"),
        mode: 0o777,
        content: Content::Link(format!("/lib/ferrix/units/{UNIT}")),
    }
}

/// The flood's unit: after lode, its output through init's log pipe.
const TEST_UNIT: &str = "\
# test-log's flood (tools/common/xtask/src/lode.rs).
[Unit]
Description=Flood the kernel log and kill lode in the middle
After=lode.service

[Service]
Type=oneshot
ExecStart=/bin/busybox sh /lib/lode-test.sh
";

/// The marker the flood's unit ends with.
const DONE: &str = "lode-test: done";

/// The flood. Every line it says goes through init's log pipe, so the
/// unit's lines reach lode as `lode-test.service`'s.
const TEST_SCRIPT: &str = r#"bb=/bin/busybox
say() { echo "lode-test: $*"; }
i=0
until [ -n "$($bb pidof lode)" ]; do
  i=$((i+1)); [ $i -gt 200 ] && { say "FAIL lode never ran"; say done; exit 1; }
  $bb usleep 100000
done
first=$($bb pidof lode)
/bin/lode mark "flood begins" || say "FAIL lode mark"
n=0
while [ $n -lt 3000 ]; do
  echo "<14>lode flood $n" > /dev/kmsg
  n=$((n+1))
  if [ $n -eq 1000 ]; then $bb kill -9 $first && say "killed lode $first"; fi
done
/bin/lode mark "flood ends"
i=0
until /bin/lode -b -g "lode flood 2999" -o cat | $bb grep -q "lode flood 2999"; do
  i=$((i+1)); [ $i -gt 300 ] && { say "FAIL the flood's last line was never stored"; break; }
  $bb usleep 100000
done
say "restarted as $($bb pidof lode)"
say "verify: $(/bin/lode verify --kernel -b | $bb tail -n 1)"
say "stored: $(/bin/lode -b --since-mark "flood begins" --until-mark "flood ends" -g "lode flood " -o cat | $bb grep -c "^lode flood ") of 3000"
/bin/lode -b -g "lode flood " -o cat | $bb awk '/^lode flood /{seen[$3]=1} END{for(i=0;i<3000;i++) if(!(i in seen)) printf "%d ", i}' > /tmp/missing
[ -s /tmp/missing ] && say "missing: $($bb cat /tmp/missing)"
for m in $($bb cat /tmp/missing); do /bin/lode -b -g "flood $m" -o json | while read -r l; do say "record: $l"; done; done
/bin/lode -b -g "lode flood " -o cat | $bb awk '/^lode flood /{c[$3]++} END{for(i in c) if(c[i]>1) printf "%s ", i}' > /tmp/twice
[ -s /tmp/twice ] && say "twice: $($bb cat /tmp/twice)"
if [ -s /tmp/missing ]; then m=$($bb cat /tmp/missing | $bb awk '{print $1}'); /bin/lode -b -o json | $bb grep -n "lode flood $((m-1))\"" -A3 | while read -r l; do say "near: $l"; done; fi
say "restart records: $(/bin/lode -b -t lode -g "lode restarted" -o cat | $bb grep -c "lode restarted")"
say "unit: $(/bin/lode -b -u lode-test -g "killed lode" -o json | $bb grep -c '"UNIT":"lode-test.service"')"
pid=$($bb pidof lode)
say "uid: $($bb grep '^Uid:' /proc/$pid/status | $bb awk '{print $2}')"
say "cgroup: $($bb cat /proc/$pid/cgroup)"
if echo $pid > /sys/fs/cgroup/cgroup.procs 2>/dev/null; then say "sealed: moved"; else say "sealed: refused"; fi
say "boots: $(/bin/lode --list-boots | $bb tail -n +2 | $bb wc -l)"
say "kernel records: $(/bin/lode -b -k -o cat | $bb wc -l)"
say done
"#;

/// `cargo xtask test-log`.
fn test_log(args: &Args) -> Result<()> {
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
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox),
    });
    files.push(enabled());
    // No shell for a console getty to start: masked, so its retries do not
    // write half lines into the log the flood is counted in.
    files.push(File {
        path: "etc/ferrix/units/getty@.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/dev/null".to_owned()),
    });
    files.push(File {
        path: "lib/lode-test.sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(TEST_SCRIPT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/lode-test.service".to_owned(),
        mode: 0o644,
        content: Content::Bytes(TEST_UNIT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/multi-user.target.wants/lode-test.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/lode-test.service".to_owned()),
    });
    println!("  {arch}: building an image whose init starts lode");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = format!("{} log_buf_len=1M\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!("  {arch}: flooding the kernel log and killing lode in the middle");
    let lines = qemu::watch_lines(arch, &image, &kernel, args, DONE)?;
    judge(&lines).map_err(|why| {
        Error::new(format!(
            "{arch}: lode lost records or did not say so: {why}\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!(
        "  {arch}: lode kept every kernel record across a SIGKILL mid-flood, or said which it lost"
    );
    Ok(())
}

/// The value after `what` on the line that has it.
fn said<'a>(lines: &'a [String], what: &str) -> Option<&'a str> {
    lines
        .iter()
        .find_map(|line| line.split_once(what).map(|(_, rest)| rest.trim()))
}

/// What the flood's unit said, judged.
fn judge(lines: &[String]) -> std::result::Result<(), String> {
    if let Some(line) = lines.iter().find(|line| line.contains("lode-test: FAIL")) {
        return Err(line.trim().to_owned());
    }
    if said(lines, "lode-test: killed lode").is_none() {
        return Err("the flood never killed lode".into());
    }
    let verify = said(lines, "lode-test: verify:").ok_or("no verify line")?;
    if !verify.contains("unsaid gaps: none") {
        return Err(format!("kernel records went missing unsaid: {verify}"));
    }
    let restarts: u32 = verify
        .split(", ")
        .find_map(|part| part.strip_suffix(" restart(s)"))
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let records: u32 = said(lines, "lode-test: restart records:")
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if restarts == 0 || records == 0 {
        return Err(format!("no restart was recorded: {verify}"));
    }
    let stored: u32 = said(lines, "lode-test: stored:")
        .and_then(|text| text.split_whitespace().next())
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    let lost = verify.contains(" 0 said lost");
    if stored == 0 || (stored < 3000 && lost) {
        return Err(format!(
            "{stored} of 3000 flood lines stored, and none said lost: {verify}"
        ));
    }
    if said(lines, "lode-test: unit:").is_none_or(|count| count == "0") {
        return Err("the unit's own lines were not stored under lode-test.service".into());
    }
    isolated(lines)
}

/// The service's box: its own user, its job under the sealed
/// `drivers.slice`, a move out of it refused; one boot, kernel records.
fn isolated(lines: &[String]) -> std::result::Result<(), String> {
    let uid = said(lines, "lode-test: uid:").unwrap_or_default();
    if uid != "91" {
        return Err(format!("lode runs as uid {uid:?}, not its own user (91)"));
    }
    let cgroup = said(lines, "lode-test: cgroup:").unwrap_or_default();
    if !cgroup.ends_with("/drivers.slice/lode.service") {
        return Err(format!(
            "lode's job is {cgroup:?}, not under the sealed drivers.slice"
        ));
    }
    if said(lines, "lode-test: sealed:") != Some("refused") {
        return Err("lode's process could be moved out of its job".into());
    }
    if said(lines, "lode-test: boots:") != Some("1") {
        return Err("--list-boots did not show the one boot".into());
    }
    match said(lines, "lode-test: kernel records:").and_then(|count| count.parse::<u32>().ok()) {
        Some(count) if count > 0 => Ok(()),
        _ => Err("-k showed no kernel records".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::judge;

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn a_clean_run_passes() {
        let run = lines(
            "lode-test: killed lode 40\n\
             lode-test: verify: kernel seq 0..3500: 3501 held, 0 said lost, 1 restart(s), unsaid gaps: none\n\
             lode-test: stored: 3000 of 3000\n\
             lode-test: restart records: 1\n\
             lode-test: unit: 1\n\
             lode-test: uid: 91\n\
             lode-test: cgroup: 0::/drivers.slice/lode.service\n\
             lode-test: sealed: refused\n\
             lode-test: boots: 1\n\
             lode-test: kernel records: 300\n",
        );
        assert_eq!(judge(&run), Ok(()));
    }

    #[test]
    fn an_unsaid_gap_fails() {
        let run = lines(
            "lode-test: killed lode 40\n\
             lode-test: verify: kernel seq 0..3500: 3400 held, 0 said lost, 1 restart(s), unsaid gaps: [(900, 1001)]\n\
             lode-test: stored: 2900 of 3000\n\
             lode-test: restart records: 1\n\
             lode-test: unit: 1\n",
        );
        assert!(judge(&run).is_err());
    }

    #[test]
    fn missing_lines_said_lost_pass() {
        let run = lines(
            "lode-test: killed lode 40\n\
             lode-test: verify: kernel seq 0..3500: 3400 held, 101 said lost, 1 restart(s), unsaid gaps: none\n\
             lode-test: stored: 2899 of 3000\n\
             lode-test: restart records: 1\n\
             lode-test: unit: 1\n\
             lode-test: uid: 91\n\
             lode-test: cgroup: 0::/drivers.slice/lode.service\n\
             lode-test: sealed: refused\n\
             lode-test: boots: 1\n\
             lode-test: kernel records: 300\n",
        );
        assert_eq!(judge(&run), Ok(()));
    }

    #[test]
    fn no_restart_fails() {
        let run = lines(
            "lode-test: killed lode 40\n\
             lode-test: verify: kernel seq 0..3500: 3501 held, 0 said lost, 0 restart(s), unsaid gaps: none\n\
             lode-test: stored: 3000 of 3000\n\
             lode-test: restart records: 0\n\
             lode-test: unit: 1\n",
        );
        assert!(judge(&run).is_err());
    }
}
