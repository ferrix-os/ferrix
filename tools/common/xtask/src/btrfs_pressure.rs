//! `test-btrfs-pressure`: a writable btrfs written past the machine's memory,
//! the way Endfield's launcher unpacks the game, then all of it deleted.
//!
//! Every other btrfs gate writes a little into a lot of memory, so the page
//! cache never fills and reclaim is never reached. On the N150 box
//! (2026-10-17) the launcher's 7-Zip was told `ERROR_DISK_FULL` with 400 GB
//! free, and the box crawled while the launcher deleted what it had unpacked
//! (`docs/BTRFS-PRESSURE.md`). This gate makes the same pressure on a small
//! machine:
//!
//! * the guest has little memory (`--memory`, 768 MiB unless given);
//! * two readers hold a 1 GiB source open and stream it, as 7-Zip holds the
//!   archive's volumes, so clean cache keeps filling the machine;
//! * three writers each write members as Wine writes a file 7-Zip extracts:
//!   one byte at the member's size (Wine's `SetEndOfFile` extend),
//!   `ftruncate` to the size, the bytes from the start in 32 KiB writes,
//!   `ftruncate` again -- members of 200 MiB to 1.2 GiB between runs of
//!   small ones, [`WRITE_MIB`] between them;
//! * then `rm -rf` of the lot, with a probe that wakes every 100 ms and
//!   records how late it woke, which must stay under [`STALL_LIMIT_CS`].
//!
//! `FERRIX_PRESSURE_BOOTS=cgroup` (or `all`) runs it with the writers and
//! readers in a cgroup at `memory.max`, half the machine: not part of the
//! gate yet, since that path fails on defects of its own
//! (`docs/BTRFS-PRESSURE.md`, "At memory.max").
//!
//! With `--cpu-quota PCT` (and `--pin CPUS`) QEMU runs in a transient
//! `systemd-run --user` scope limited to that share of a processor: a slow,
//! starved machine, as the box's three busy processors left it.
//!
//! The volume is a fresh sparse `mkfs.btrfs` image of [`VOLUME_GIB`] GiB
//! under `build/`, far bigger than what is written: any `ENOSPC` is a wrong
//! answer, not a full disk.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::qemu::{self, SUCCESS_MARKER};
use crate::task::Task;
use crate::{Error, Result, busybox, cargo, init_file, native, shell};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-btrfs-pressure",
    help_at: 171,
    help: &[
        "test-btrfs-pressure  Write GiBs through btrfs on a small machine as 7-Zip under Wine does, delete them, require no ENOSPC and no long stall",
    ],
    run: |_, args| test(args),
}];

/// Where the script is in the image.
const SCRIPT: &str = "/etc/btrfs-pressure";

/// What the guest writes in all, in MiB.
const WRITE_MIB: u64 = 6 * 1024;

/// The volume's size: four times what is written and more.
const VOLUME_GIB: u64 = 32;

/// Writers at once, each its share of [`WRITE_MIB`], and readers streaming
/// the source: 7-Zip's one extracting thread had the launcher, Wine's
/// server and its own reader beside it on four processors.
const WRITERS: u32 = 3;
const READERS: u32 = 2;

/// The guest's memory unless `--memory` says otherwise, in MiB.
const MEMORY_MIB: u32 = 768;

/// The longest a 100 ms sleep of the probe may overrun, in centiseconds.
const STALL_LIMIT_CS: u64 = 500;

/// What the guest prints when it is through.
const DONE: &str = "pressure: done";

/// The guest's script. `@WRITE_MIB@` and `@MEMMAX@` are filled in.
const GUEST: &str = r#"#!/bin/sh
B=/bin/busybox
say() { echo "pressure: $*"; }
cs() { read u r < /proc/uptime; echo $(( ${u%.*} * 100 + 1${u#*.} - 100 )); }
D=/data/p
$B mkdir -p /tmp $D || { say "FAIL mkdir $D"; echo "pressure: done"; exit 1; }
# The probe: sleep 100 ms, note how late it woke; the longest, per phase.
probe() {
  max=0; last=$(cs)
  while [ ! -e /tmp/stop.$1 ]; do
    $B sleep 0.1; now=$(cs); late=$((now - last - 10)); last=$now
    if [ $late -gt $max ]; then max=$late; echo $max > /tmp/probe.$1; fi
  done
}
# One writer: members as 7-Zip under Wine writes them, until its share is
# written or a step fails. $1 its number, $2 its share in MiB.
$B cat > /tmp/writer <<'EOW'
B=/bin/busybox
w=$1; E=/tmp/w$w.err; D=/data/p; out=$D/out$w
cs() { read u r < /proc/uptime; echo $(( ${u%.*} * 100 + 1${u#*.} - 100 )); }
# A program refused memory of its own (ENOMEM: busybox's "out of memory") is
# tried again, counted, and not a failure here: in a job at memory.max a
# kernel charge is refused without a reclaim, a known defect of its own
# (docs/BTRFS-PRESSURE.md). ENOSPC, or anything else, is.
enomem=0
step() {
  what=$1; shift; tries=0
  while :; do
    "$@" 2>$E; rc=$?; [ $rc -eq 0 ] && return 0
    if $B grep -qi "out of memory" $E && [ $tries -lt 50 ]; then
      tries=$((tries + 1)); enomem=$((enomem + 1)); $B sleep 0.2; continue
    fi
    fail="$what: status $rc: $($B cat $E)"; return 1
  done
}
fail=""; n=0; total=0; limit=$(( $2 * 1048576 ))
set -- 281932483 1258037404 626970286 897524169 1195460920 216636808 1152256693
i=0; while [ $i -lt $w ]; do size=$1; shift; set -- "$@" $size; i=$((i + 1)); done
while [ $total -lt $limit ]; do
  if [ $((n % 16)) -eq 15 ]; then size=$1; shift; set -- "$@" $size
  else size=$(( ((n + 97 * w) * 7919 * 523) % 4194304 + 1 )); fi
  dir=$out/d$((n / 64)); f=$dir/m$n
  step "mkdir $dir" $B mkdir -p $dir || break
  step "extend $f to $size" $B dd if=/dev/zero of=$f bs=1 count=1 seek=$size conv=notrunc || break
  step "truncate $f to $size" $B truncate -s $size $f || break
  blocks=$(( (size + 32767) / 32768 ))
  first=$blocks; [ $first -gt 32768 ] && first=32768
  step "write $f ($size)" $B dd if=$D/src of=$f bs=32768 count=$first conv=notrunc || break
  if [ $blocks -gt 32768 ]; then
    step "write $f past 1 GiB ($size)" $B dd if=$D/src of=$f bs=32768 seek=32768 count=$((blocks - 32768)) conv=notrunc || break
  fi
  step "truncate again $f" $B truncate -s $size $f || break
  n=$((n + 1)); total=$((total + size))
done
if [ -n "$fail" ]; then
  echo "pressure: FAIL writer $w after $n files: $fail"
  echo "pressure: at the failure: $($B df /data | $B tail -1)"
  echo "pressure: at the failure: $($B grep -E 'MemFree|^Cached|Dirty' /proc/meminfo | $B tr -s ' ' | $B tr '\n' ' ')"
  [ -e /sys/fs/cgroup/pressure/memory.events ] && echo "pressure: at the failure: memory.current $($B cat /sys/fs/cgroup/pressure/memory.current), events $($B tr '\n' ' ' < /sys/fs/cgroup/pressure/memory.events)"
fi
[ $enomem -gt 0 ] && echo "pressure: note: writer $w was refused memory of its own (ENOMEM) $enomem times and tried again"
echo "$n $total" > /tmp/w$w.done
EOW
if [ @CGROUP@ = 1 ]; then
  [ -e /sys/fs/cgroup/cgroup.procs ] || $B mount -t cgroup2 none /sys/fs/cgroup 2>/dev/null
  echo +memory > /sys/fs/cgroup/cgroup.subtree_control 2>/dev/null
  if $B mkdir -p /sys/fs/cgroup/pressure 2>/dev/null && echo @MEMMAX@ > /sys/fs/cgroup/pressure/memory.max 2>/dev/null; then
    say "writers and readers in a cgroup, memory.max @MEMMAX@"
  else
    say "FAIL no cgroup memory.max here"
  fi
  join='echo $$ > /sys/fs/cgroup/pressure/cgroup.procs;'
else
  say "no cgroup: the machine's own memory floor"
  join=''
fi
t0=$(cs)
$B dd if=/dev/zero of=$D/src bs=1048576 count=1024 2>/tmp/src.err || say "FAIL source: $($B cat /tmp/src.err)"
# Readers hold the source open and stream it, as 7-Zip holds the archive.
r=0; while [ $r -lt @READERS@ ]; do
  $B sh -c "$join exec 3<$D/src; while [ ! -e /tmp/stop.read ]; do $B dd if=$D/src of=/dev/null bs=1048576 2>/dev/null; done" &
  r=$((r + 1))
done
probe write &
w=0; while [ $w -lt @WRITERS@ ]; do
  $B sh -c "$join exec $B sh /tmp/writer $w $(( @WRITE_MIB@ / @WRITERS@ ))" &
  w=$((w + 1))
done
# Wait for the writers, saying every 15 s how far they got: a writer that
# stops moving is seen as such, not only at the timeout.
beat=0
w=0; while [ $w -lt @WRITERS@ ]; do
  while [ ! -e /tmp/w$w.done ]; do
    $B sleep 1; beat=$((beat + 1))
    if [ $((beat % 15)) -eq 0 ]; then
      say "after $beat s: $($B du -sm $D 2>/dev/null | $B cut -f1) MiB on /data, MemFree $($B grep MemFree /proc/meminfo | $B tr -s ' ' | $B cut -d' ' -f2) kB"
    fi
  done
  w=$((w + 1))
done
$B touch /tmp/stop.write /tmp/stop.read
wait
n=0; total=0; w=0
while [ $w -lt @WRITERS@ ]; do read a b < /tmp/w$w.done; n=$((n + a)); total=$((total + b)); w=$((w + 1)); done
say "wrote $n files, $((total / 1048576)) MiB in $(( ($(cs) - t0) / 100 )) s; longest probe stall $($B cat /tmp/probe.write 2>/dev/null || echo 0) cs"
$B sync 2>/tmp/sync.err || say "FAIL sync: $($B cat /tmp/sync.err)"
probe delete &
t1=$(cs)
$B rm -rf $D 2>/tmp/rm.err || say "FAIL rm: $($B cat /tmp/rm.err)"
$B sync 2>/tmp/sync.err || say "FAIL sync after rm: $($B cat /tmp/sync.err)"
t2=$(cs)
$B touch /tmp/stop.delete
wait
say "deleted it all in $(( (t2 - t1) / 100 )) s; longest probe stall $($B cat /tmp/probe.delete 2>/dev/null || echo 0) cs"
say "free after: $($B grep MemFree /proc/meminfo | $B tr -s ' ' | $B cut -d' ' -f2) kB"
echo "pressure: done"
$B poweroff -f
"#;

/// How many MiB of memory the eviction boot reads into the cache per 64 MiB
/// file it writes first: one file per 64 MiB of the machine, as much as it
/// has, so that it starts at its floor, as the N150 box did with 16 GB of
/// game files cached (2026-10-18).
const EVICT_FILE_MIB: u64 = 64;

/// How many small files the eviction boot then makes, as `tar` unpacking
/// GE-Proton makes them: twice the VFS's dentry cache (`DEFAULT_CACHE`).
const EVICT_NEW_FILES: u64 = 8192;

/// The eviction boot's script: the cache filled by reading, then a burst of
/// small files made fast, with the probe running and `Cached` sampled. On the
/// N150 box the same burst took `Cached` from 15.97 GB to 0.13 GB in 13 s.
const EVICT_GUEST: &str = r#"#!/bin/sh
B=/bin/busybox
say() { echo "pressure: $*"; }
cs() { read u r < /proc/uptime; echo $(( ${u%.*} * 100 + 1${u#*.} - 100 )); }
cached() { while read k v u; do [ "$k" = Cached: ] && { echo $v; return; }; done < /proc/meminfo; }
D=/data/e
$B mkdir -p /tmp $D || { say "FAIL mkdir $D"; echo "pressure: done"; exit 1; }
probe() {
  max=0; last=$(cs)
  while [ ! -e /tmp/stop.$1 ]; do
    $B sleep 0.1; now=$(cs); late=$((now - last - 10)); last=$now
    if [ $late -gt $max ]; then max=$late; echo $max > /tmp/probe.$1; fi
  done
}
i=0; while [ $i -lt @FILES@ ]; do
  $B dd if=/dev/zero of=$D/big$i bs=1048576 count=@FILE_MIB@ 2>/tmp/dd.err || say "FAIL fill big$i: $($B cat /tmp/dd.err)"
  i=$((i + 1))
done
$B sync
# Read them back, the newest last: what the machine holds is clean cache.
i=0; while [ $i -lt @FILES@ ]; do $B cat $D/big$i > /dev/null; i=$((i + 1)); done
before=$(cached)
say "evict: cache filled: Cached $before kB, MemFree $($B grep MemFree /proc/meminfo | $B tr -s ' ' | $B cut -d' ' -f2) kB"
echo $before > /tmp/least
( while [ ! -e /tmp/stop.unpack ]; do
    c=$(cached); read l < /tmp/least; [ $c -lt $l ] && echo $c > /tmp/least
    $B sleep 0.2
  done ) &
probe unpack &
# The unpack: small files in directories of 256, made as fast as the shell
# can, each a page of text, as tar makes a tree of them.
page=0123456789abcdef; k=0; while [ $k -lt 8 ]; do page=$page$page; k=$((k + 1)); done
t0=$(cs); n=0
while [ $n -lt @NEW@ ]; do
  if [ $((n % 256)) -eq 0 ]; then $B mkdir -p $D/u/d$((n / 256)) || { say "FAIL mkdir d$((n / 256))"; break; }; fi
  echo "$page" > $D/u/d$((n / 256))/f$n || { say "FAIL create f$n"; break; }
  n=$((n + 1))
done
$B sync
t1=$(cs)
$B touch /tmp/stop.unpack
wait
after=$(cached); read least < /tmp/least; [ $after -lt $least ] && least=$after
say "evict: made $n files in $(( (t1 - t0) / 100 )) s; Cached before $before kB, after $after kB, least $least kB; longest probe stall $($B cat /tmp/probe.unpack 2>/dev/null || echo 0) cs"
echo "pressure: done"
$B poweroff -f
"#;

/// The most of the cache the eviction boot may lose while it makes its small
/// files, in percent of what was cached before: they take a few tens of
/// megabytes, so losing half of a 600 MB cache is not reclaim's shortfall.
const EVICT_LOSS_PERCENT: u64 = 50;

/// Boot each architecture asked for and judge it.
///
/// # Errors
///
/// A boot that panicked or never finished, a failed step, or a stall over
/// the limit.
pub(crate) fn test(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        run(arch, args)?;
    }
    Ok(())
}

fn run(arch: Arch, args: &Args) -> Result<()> {
    let init = args.init.as_deref().unwrap_or(busybox::INIT_NAME);
    let program = crate::program_for(init, arch)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel_with_init(arch, args.release, &program, shell::SCRIPT)?;
    let natives = native::build(arch, args.release)?;
    let carried = shell::carried_for(arch, &program, args)?;
    let parts = init_file::Parts {
        arch,
        loader: &loader,
        kernel: &kernel,
        natives: &natives,
        program: &program,
        carried: &carried,
    };
    let shell_bytes = std::fs::read(&program)
        .map_err(|error| Error::new(format!("reading {}: {error}", program.display())))?;
    let mut machine = args.clone();
    if !machine.memory_given {
        machine.memory = MEMORY_MIB;
    }
    if !machine.timeout_given {
        machine.timeout = 1800;
    }
    let mut failures = Vec::new();
    // The machine's floor and the eviction under a burst of small files are
    // the gate. The boot at `memory.max` is run when `FERRIX_PRESSURE_BOOTS`
    // names `cgroup` (or says `all`): it fails on main and with this fix
    // alike, on defects of the memory.max path of their own
    // (docs/BTRFS-PRESSURE.md, "At memory.max"). The variable is a comma list
    // of `floor`, `evict` and `cgroup`.
    let boots = std::env::var("FERRIX_PRESSURE_BOOTS").unwrap_or_else(|_| "floor,evict".to_owned());
    for kind in [Kind::Floor, Kind::Evict, Kind::Cgroup] {
        if boots != "all" && !boots.split(',').any(|name| name.trim() == kind.name()) {
            continue;
        }
        if let Err(error) = boot(&parts, &shell_bytes, &kernel, &machine, kind) {
            println!("  {arch}: {error}");
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(Error::new(failures.join("\n")))
    }
}

/// Which boot: the writers at the machine's own floor, the cache filled and
/// then a burst of small files, or the writers in a job at `memory.max`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Floor,
    Evict,
    Cgroup,
}

impl Kind {
    /// Its name in `FERRIX_PRESSURE_BOOTS`.
    fn name(self) -> &'static str {
        match self {
            Kind::Floor => "floor",
            Kind::Evict => "evict",
            Kind::Cgroup => "cgroup",
        }
    }
}

/// One boot of `kind`.
fn boot(
    parts: &init_file::Parts<'_>,
    shell_bytes: &[u8],
    kernel: &cargo::Kernel,
    machine: &Args,
    kind: Kind,
) -> Result<()> {
    let arch = parts.arch;
    let cgroup = kind == Kind::Cgroup;
    let memmax = u64::from(machine.memory) * 1024 * 1024 / 2;
    let script = if kind == Kind::Evict {
        EVICT_GUEST
            .replace("@FILES@", &evict_files(&machine).to_string())
            .replace("@NEW@", &EVICT_NEW_FILES.to_string())
            .replace("@FILE_MIB@", &EVICT_FILE_MIB.to_string())
    } else {
        GUEST
            .replace("@WRITE_MIB@", &WRITE_MIB.to_string())
            .replace("@MEMMAX@", &memmax.to_string())
            .replace("@CGROUP@", if cgroup { "1" } else { "0" })
            .replace("@WRITERS@", &WRITERS.to_string())
            .replace("@READERS@", &READERS.to_string())
    };
    let image = parts.image_with(
        shell_bytes,
        SCRIPT,
        &script,
        &qemu::init_option(SCRIPT),
        &["bin/busybox"],
    )?;
    let mut machine = machine.clone();
    machine.data_image = Some(volume(arch)?);
    machine.data_image_kept = true;
    if kind == Kind::Evict {
        println!(
            "  {arch}: {} MiB of memory, the cache filled with {} files of {EVICT_FILE_MIB} MiB, \
             then {EVICT_NEW_FILES} small files made",
            machine.memory,
            evict_files(&machine)
        );
    } else {
        println!(
            "  {arch}: {} MiB of memory{}, {WRITE_MIB} MiB written as 7-Zip under Wine writes, \
             then deleted{}",
            machine.memory,
            if cgroup {
                format!(", the writer at memory.max {} MiB", memmax >> 20)
            } else {
                String::new()
            },
            machine.cpu_quota.map_or(String::new(), |q| format!(
                ", QEMU held to {q}% of a processor"
            ))
        );
    }
    let started = Instant::now();
    let deadline = started + Duration::from_secs(machine.timeout);
    let lines = qemu::watch_then(arch, &image, kernel, &machine, SUCCESS_MARKER, |at| {
        let _ = at.read_more(deadline, |lines| lines.iter().any(|l| l.trim() == DONE))?;
        Ok(())
    })?;
    let took = started.elapsed().as_secs();
    let ours: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("pressure: "))
        .collect();
    for line in &ours {
        println!("  {arch}: {line}");
    }
    println!("  {arch}: the boot took {took} s");
    let judged = if kind == Kind::Evict {
        judge_evict(&ours)
    } else {
        judge(&ours)
    };
    judged.map_err(|why| Error::new(format!("{arch}: {why}")))
}

/// Whether the guest finished, nothing failed, and no probe stalled long.
fn judge(ours: &[&str]) -> std::result::Result<(), String> {
    if !ours.contains(&DONE) {
        return Err(format!("the guest never said `{DONE}`"));
    }
    if let Some(failed) = ours.iter().find(|l| l.starts_with("pressure: FAIL")) {
        return Err(format!("a step failed: {failed}"));
    }
    for phase in ["wrote ", "deleted "] {
        let line = ours
            .iter()
            .find(|l| l.starts_with(&format!("pressure: {phase}")))
            .ok_or_else(|| format!("no `{phase}` line"))?;
        let stall = line
            .rsplit("longest probe stall ")
            .next()
            .and_then(|rest| rest.trim_end_matches(" cs").parse::<u64>().ok())
            .ok_or_else(|| format!("no stall figure in `{line}`"))?;
        if stall > STALL_LIMIT_CS {
            return Err(format!(
                "a 100 ms sleep woke {stall} cs late ({line}); the limit is {STALL_LIMIT_CS}"
            ));
        }
    }
    Ok(())
}

/// How many files the eviction boot fills the cache with: one per
/// [`EVICT_FILE_MIB`] of the machine's memory.
fn evict_files(machine: &Args) -> u64 {
    (u64::from(machine.memory) / EVICT_FILE_MIB).max(1)
}

/// Whether the eviction boot finished, nothing failed, the cache kept at
/// least half of what it held at its least, and no probe stalled long.
fn judge_evict(ours: &[&str]) -> std::result::Result<(), String> {
    if !ours.contains(&DONE) {
        return Err(format!("the guest never said `{DONE}`"));
    }
    if let Some(failed) = ours.iter().find(|l| l.starts_with("pressure: FAIL")) {
        return Err(format!("a step failed: {failed}"));
    }
    let line = ours
        .iter()
        .find(|l| l.starts_with("pressure: evict: made "))
        .ok_or("no `evict: made` line")?;
    let figure = |after: &str, unit: &str| {
        line.split(after)
            .nth(1)
            .and_then(|rest| rest.split(unit).next())
            .and_then(|n| n.trim().parse::<u64>().ok())
            .ok_or_else(|| format!("no `{after}` figure in `{line}`"))
    };
    let before = figure("Cached before ", " kB")?;
    let least = figure("least ", " kB")?;
    let stall = figure("longest probe stall ", " cs")?;
    if least.saturating_mul(100) < before.saturating_mul(100 - EVICT_LOSS_PERCENT) {
        return Err(format!(
            "the page cache fell from {before} kB to {least} kB while small files were made \
             ({line}); it may lose at most {EVICT_LOSS_PERCENT}%"
        ));
    }
    if stall > STALL_LIMIT_CS {
        return Err(format!(
            "a 100 ms sleep woke {stall} cs late ({line}); the limit is {STALL_LIMIT_CS}"
        ));
    }
    Ok(())
}

/// A fresh sparse volume of [`VOLUME_GIB`] GiB, made by the host's
/// `mkfs.btrfs` with its default profiles and no label.
fn volume(arch: Arch) -> Result<PathBuf> {
    let path = paths::build_dir(arch).join("btrfs-pressure.img");
    let _ = std::fs::remove_file(&path);
    let file = std::fs::File::create(&path)
        .map_err(|error| Error::new(format!("creating {}: {error}", path.display())))?;
    file.set_len(VOLUME_GIB << 30)
        .map_err(|error| Error::new(format!("sizing {}: {error}", path.display())))?;
    drop(file);
    mkfs(&path)?;
    Ok(path)
}

fn mkfs(path: &Path) -> Result<()> {
    let status = Command::new("mkfs.btrfs")
        .args(["-q", "-f"])
        .arg(path)
        .status()
        .map_err(|error| {
            Error::new(format!(
                "mkfs.btrfs did not run ({error}); test-btrfs-pressure needs btrfs-progs"
            ))
        })?;
    if !status.success() {
        return Err(Error::new(format!("mkfs.btrfs {} failed", path.display())));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_run_passes_and_a_failed_step_does_not() {
        let good = [
            "pressure: wrote 10 files, 6144 MiB in 100 s; longest probe stall 30 cs",
            "pressure: deleted it all in 5 s; longest probe stall 40 cs",
            DONE,
        ];
        assert_eq!(judge(&good), Ok(()));
        let full = [
            "pressure: FAIL write /data/p/out/d0/m15 (281932483): dd: error writing: No space left on device",
            "pressure: wrote 15 files, 300 MiB in 10 s; longest probe stall 3 cs",
            "pressure: deleted it all in 1 s; longest probe stall 4 cs",
            DONE,
        ];
        assert!(judge(&full).is_err());
        let stalled = [
            "pressure: wrote 10 files, 6144 MiB in 100 s; longest probe stall 30 cs",
            "pressure: deleted it all in 50 s; longest probe stall 2400 cs",
            DONE,
        ];
        assert!(judge(&stalled).is_err());
    }

    #[test]
    fn the_eviction_boot_fails_on_a_cache_dropped_whole_or_a_stall() {
        let kept = [
            "pressure: evict: made 8192 files in 20 s; Cached before 600000 kB, after 560000 kB, least 540000 kB; longest probe stall 20 cs",
            DONE,
        ];
        assert_eq!(judge_evict(&kept), Ok(()));
        let dropped = [
            "pressure: evict: made 8192 files in 20 s; Cached before 600000 kB, after 40000 kB, least 30000 kB; longest probe stall 20 cs",
            DONE,
        ];
        assert!(judge_evict(&dropped).is_err());
        let stalled = [
            "pressure: evict: made 8192 files in 20 s; Cached before 600000 kB, after 560000 kB, least 540000 kB; longest probe stall 900 cs",
            DONE,
        ];
        assert!(judge_evict(&stalled).is_err());
        assert!(judge_evict(&[DONE]).is_err());
    }
}
