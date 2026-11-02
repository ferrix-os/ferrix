//! `cargo xtask test-quarantine`: an unstable core taken out of service by
//! lode and brought back by root (`docs/CORE-QUARANTINE.md` §7).
//!
//! The image's init starts lode with the default policy (`action =
//! quarantine`, ten core-scoped records in ten minutes) and a unit that
//! keeps four CPU-bound loops running. Once it says it is ready, QEMU's
//! monitor injects eleven corrected level 1 data cache errors into
//! processor 2's bank 1, one a poll; lode must take the processor out
//! through its token. The unit then reads the Linux surface -- `cpu2/online`
//! 0, `online` without 2, `nproc` one less, no task's `/proc/<pid>/stat` on
//! processor 2, `/proc/ferrix-service` saying `service out drained 0` -- brings
//! the processor back as root with `echo 1`, and reads `online` whole again.

use std::time::{Duration, Instant};

use crate::args::Args;
use crate::display::{Qmp, free_port};
use crate::machine_check::{INTEL, monitor, refusal};
use crate::paths::Arch;
use crate::ports::{Content, File};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, lode, native, qemu};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[
    Task {
        name: "test-quarantine",
        help_at: 227,
        help: &[
            "test-quarantine  Inject eleven core-scoped corrected errors into x86_64 processor 2 under init and lode;",
            "              lode must take it out of service, the Linux surface say so, and root bring it back",
        ],
        run: |_, args| test_quarantine(args, FOUR),
    },
    Task {
        name: "test-quarantine-smp2",
        help_at: 228,
        help: &[
            "test-quarantine-smp2  test-quarantine on two processors: processor 1 goes, the boot processor runs alone",
        ],
        run: |_, args| test_quarantine(args, TWO),
    },
    Task {
        name: "test-quarantine-unremapped",
        help_at: 229,
        help: &[
            "test-quarantine-unremapped  With VT-d's interrupt remapping off, root's take-out of a processor holding a",
            "              compatibility-format message is refused, the processor left in service",
        ],
        run: |_, args| test_unremapped(args),
    },
];

/// A boot of the gate: how many processors, and the one the errors go to.
#[derive(Clone, Copy, Debug)]
struct Mode {
    /// Processors.
    smp: usize,
    /// The one taken out.
    target: usize,
}

/// Four processors, processor 2 taken out; the probes write processor 1's
/// file.
const FOUR: Mode = Mode { smp: 4, target: 2 };

/// Two processors: processor 1 goes and the boot processor runs alone. No
/// probes: no processor is left to write to.
const TWO: Mode = Mode { smp: 2, target: 1 };

impl Mode {
    /// The secondary processors, as the script's list.
    fn others(self) -> String {
        (1..self.smp)
            .map(|cpu| cpu.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The processor the probes write to: a secondary other than the
    /// target, if there is one.
    fn probe(self) -> Option<usize> {
        (1..self.smp).find(|&cpu| cpu != self.target)
    }

    /// The guest's script for this boot.
    fn script(self) -> String {
        SCRIPT
            .replace("@T@", &self.target.to_string())
            .replace("@OTHERS@", &self.others())
            .replace(
                "@PROBE@",
                &self.probe().map_or_else(String::new, |cpu| cpu.to_string()),
            )
    }

    /// The processors in service while the target is out, as sysfs lists them.
    fn without_target(self) -> String {
        cpu_list(
            &(0..self.smp)
                .filter(|&cpu| cpu != self.target)
                .collect::<Vec<_>>(),
        )
    }

    /// Every processor, as sysfs lists them.
    fn all(self) -> String {
        cpu_list(&(0..self.smp).collect::<Vec<_>>())
    }
}

/// Processors as sysfs lists them: ranges joined by commas, `0-1,3`.
fn cpu_list(cpus: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut at = 0;
    while let Some(&start) = cpus.get(at) {
        let mut end = start;
        while cpus.get(at + 1) == Some(&(end + 1)) {
            end += 1;
            at += 1;
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        at += 1;
    }
    parts.join(",")
}

/// The bank they go to: a core's own on every model QEMU offers.
const BANK: u32 = 1;

/// `MCi_STATUS` of a corrected level 1 data cache read error: `VAL`, `EN`,
/// `MISCV`, `ADDRV`, MCA code 0x0145 (SDM vol. 3B 17.9.2, `0000 0001 RRRR
/// TTLL` with `RRRR` data read, `TT` data, `LL` level 1).
const CORE_SCOPED: u64 = 0x9c00_0000_0000_0145;

/// `MCi_MISC`: a page's worth of address bits, physical address mode.
const MISC_PAGE: u64 = 0x8c;

/// How many: one past the default count of ten.
const INJECTIONS: usize = 11;

/// One a poll, which the image asks for every second.
const SPACING: Duration = Duration::from_millis(1200);

/// The unit's line once its loops run.
const READY: &str = "quarantine-test: ready";

/// The unit's last line.
const DONE: &str = "quarantine-test: done";

/// How long the guest is given after the last injection.
const PATIENCE: Duration = Duration::from_secs(120);

/// The probes' program (`src/tests/quarantine`).
const PROBE: &str = "quarantine";

/// The unit that runs [`SCRIPT`] once lode is up.
const UNIT: &str = "\
# test-quarantine's guest half (tools/common/xtask/src/quarantine.rs).
[Unit]
Description=Keep processors busy, then read what lode's quarantine did
After=lode.service

[Service]
Type=oneshot
TimeoutStartSec=600
ExecStart=/bin/busybox sh /lib/quarantine-test.sh
";

/// The guest half; every line it prints starts with `quarantine-test:`.
///
/// Before `ready`: four CPU-bound loops, and 24 readers blocked on empty
/// pipes -- a wait on no list, which only the re-home at the wake finds --
/// how many are homed on the target said (`/proc/<pid>/stat` field 39),
/// and `/proc/interrupts` said. Then it waits up to a minute for processor
/// 2 to go and reads the surface; says `/proc/interrupts` again, makes
/// disk and timer traffic, and says it a third time; ends the readers
/// with a signal, whose wake has no waker on their side and so re-homes
/// each one homed on 2 (L.sched.103);
/// runs the two probes; brings the target back as root; takes the
/// secondary processors
/// 1 to 3 out and back fifty times with the loops running; and says the
/// longest move's hold.
const SCRIPT: &str = r#"
say() { echo "quarantine-test: $*"; }
cpu=/sys/devices/system/cpu
proc_of() { busybox cut -d' ' -f39 "/proc/$1/stat" 2>/dev/null; }
interrupts() { busybox sed "s/^/quarantine-test: $1 /" /proc/interrupts; }
busy=
for n in 1 2 3 4; do busybox sh -c 'while :; do :; done' & busy="$busy $!"; done
# L.sched.123: a spinner pinned to the target by sched_setaffinity, which
# must not refuse the park, and runs elsewhere while it is out.
busybox mkdir -p /run/qt
/bin/quarantine-test pin @T@ /run/qt/pinned & pinned=$!
# Where the pinned spinner runs: `stat`'s processor field names the reader's.
pinned_on() {
    a=$(busybox cat /run/qt/pinned 2>/dev/null)
    busybox sleep 0.3
    b=$(busybox cat /run/qt/pinned 2>/dev/null)
    if [ -z "$b" ] || [ "$a" = "$b" ]; then echo stalled
    elif [ "${b#* }" = @T@ ]; then echo target
    else echo elsewhere
    fi
}
busybox mkdir -p /run/qt
for n in $(busybox seq 24); do
    busybox sh -c 'busybox sleep 100000 | busybox cat' &
done
busybox sleep 1
readers() {
    for dir in /proc/[0-9]*; do
        [ "$(busybox tr '\0' ' ' < $dir/cmdline 2>/dev/null)" = "busybox cat " ] &&
            busybox cut -d' ' -f1,39 $dir/stat
    done
}
homed=$(readers | busybox grep -c ' @T@$')
say "readers $(readers | busybox wc -l) homed on @T@: $homed"
interrupts before
say ready
i=0
while [ "$(busybox cat $cpu/cpu@T@/online)" = 1 ] && [ $i -lt 600 ]; do
    busybox sleep 0.1
    i=$((i + 1))
done
say "cpu@T@ online $(busybox cat $cpu/cpu@T@/online)"
say "online $(busybox cat $cpu/online)"
say "offline $(busybox cat $cpu/offline)"
say "nproc $(busybox nproc)"
on2=0
read=0
for stat in /proc/[0-9]*/stat; do
    p=$(busybox cut -d' ' -f39 "$stat" 2>/dev/null)
    case "$p" in [0-9]*) read=$((read + 1)) ;; esac
    [ "$p" = @T@ ] && on2=$((on2 + 1))
done
say "tasks read: $read"
say "tasks on @T@: $on2"
say "pinned mask out $(/bin/quarantine-test mask $pinned)"
say "pinned while out: $(pinned_on)"
interrupts out
disks=$(busybox ls /dev | busybox grep -E '^(vd[a-z]+|sd[a-z]+|nvme[0-9]+n[0-9]+)$' | busybox tr '\n' ' ')
say "disks $disks"
for round in 1 2 3; do
    for d in $disks; do
        busybox dd if=/dev/$d of=/dev/null bs=4k count=256 skip=$((round * 1024)) 2>/dev/null
    done
done
busybox sleep 1
interrupts traffic
busybox kill -9 $(readers | busybox cut -d' ' -f1) 2>/dev/null
busybox sleep 1
say "readers left: $(readers | busybox wc -l)"
say "service $(busybox grep '^cpu@T@ ' /proc/ferrix-service)"
say "siblings $(busybox cat $cpu/cpu@T@/topology/thread_siblings_list)"
for probe in @PROBE@; do
    /bin/quarantine-test nonroot $cpu/cpu$probe/online | busybox sed 's/^/quarantine-test: /'
    /bin/quarantine-test userns $cpu/cpu$probe/online | busybox sed 's/^/quarantine-test: /'
done
say "after probes $(busybox cat $cpu/online)"
echo 1 > $cpu/cpu@T@/online
say "back $?"
say "pinned mask back $(/bin/quarantine-test mask $pinned)"
i=0
where=$(pinned_on)
while [ "$where" != target ] && [ $i -lt 10 ]; do
    where=$(pinned_on)
    i=$((i + 1))
done
say "pinned after the return: $where"
say "online after $(busybox cat $cpu/online)"
failures=0
for i in $(busybox seq 50); do
    for c in @OTHERS@; do echo 0 > $cpu/cpu$c/online || failures=$((failures + 1)); done
    for c in @OTHERS@; do echo 1 > $cpu/cpu$c/online || failures=$((failures + 1)); done
done
alive=0
for pid in $busy; do busybox kill -0 $pid 2>/dev/null && alive=$((alive + 1)); done
say "cycles 50 failures $failures loops alive $alive online $(busybox cat $cpu/online)"
say "$(busybox grep '^move-hold-max-ns' /proc/ferrix-service)"
busybox kill $busy $pinned 2>/dev/null
say done
"#;

/// What an image carries beside the kernel: init, busybox, lode started,
/// the probe, and the guest's unit running `script`.
pub(crate) fn image_files(arch: Arch, script: String) -> Result<Vec<File>> {
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
    files.push(lode::enabled());
    let probe = crate::sem::build_test(PROBE, arch, None, false)?;
    files.push(File {
        path: "bin/quarantine-test".to_owned(),
        mode: 0o755,
        content: Content::Bytes(
            std::fs::read(&probe)
                .map_err(|error| Error::new(format!("reading the probe: {error}")))?,
        ),
    });
    // No shell for a console getty to start: masked, as test-log does.
    files.push(File {
        path: "etc/ferrix/units/getty@.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/dev/null".to_owned()),
    });
    files.push(File {
        path: "lib/quarantine-test.sh".to_owned(),
        mode: 0o755,
        content: Content::Bytes(script.into_bytes()),
    });
    files.push(File {
        path: "etc/ferrix/units/quarantine-test.service".to_owned(),
        mode: 0o644,
        content: Content::Bytes(UNIT.as_bytes().to_vec()),
    });
    files.push(File {
        path: "etc/ferrix/units/multi-user.target.wants/quarantine-test.service".to_owned(),
        mode: 0o777,
        content: Content::Link("/etc/ferrix/units/quarantine-test.service".to_owned()),
    });
    Ok(files)
}

/// Build the image, inject, and judge.
///
/// Verifies: H.SCHED.18, L.sched.102, L.sched.103, L.sched.104, L.sched.106
/// Verifies: L.smp.35, L.smp.38, L.smp.39, L.iommu.102, L.iommu.103
/// Verifies: L.sched.123
fn test_quarantine(args: &Args, mode: Mode) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-quarantine: x86_64 only; skipped");
        return Ok(());
    }
    let files = image_files(arch, mode.script())?;
    println!(
        "  {arch}: building an image whose init starts lode, polling machine checks each second"
    );
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = format!("{} ferrix.mce.poll=1\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let port = free_port()?;
    let mut qemu_args = args.clone();
    qemu_args.qmp_port = Some(port);
    // Intel's rules: the record's MCA code is what lode reads.
    qemu_args.x86_cpu_extra = Some(INTEL.to_owned());
    qemu_args.smp = mode.smp as u32;
    qemu_args.smp_given = true;
    if !args.timeout_given {
        qemu_args.timeout = 300;
    }
    let hook = |watching: &mut qemu::Watching<'_>| -> Result<()> {
        let mut qmp = Qmp::connect(port, Instant::now() + Duration::from_secs(10))?;
        let target = mode.target;
        let command = format!("mce {target} {BANK} {CORE_SCOPED:#x} 0 0x1000 {MISC_PAGE:#x}");
        for _ in 0..INJECTIONS {
            let reply = monitor(&mut qmp, &command, None)?;
            if let Some(problem) = refusal(&reply) {
                return Err(Error::new(format!(
                    "the monitor did not inject `{command}`: {problem}"
                )));
            }
            std::thread::sleep(SPACING);
        }
        println!("  {arch}: injected `{command}` {INJECTIONS} times, one a poll");
        let _ = watching.read_more(Instant::now() + PATIENCE, |seen| {
            seen.iter().any(|line| line.contains(DONE))
                || seen.iter().any(|line| line.contains(qemu::PANIC_MARKER))
        })?;
        watching.stop_when_done();
        Ok(())
    };
    let lines = qemu::watch_then(arch, &image, &kernel, &qemu_args, READY, hook)?;
    judge(&lines, mode).map_err(|why| {
        Error::new(format!(
            "{arch}: test-quarantine: {why}.\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    let hold = lines
        .iter()
        .find_map(|line| line.split_once("move-hold-max-ns "))
        .map_or("?", |(_, ns)| ns.trim());
    println!(
        "  {arch}: test-quarantine ({} processors): lode took processor {} out after {INJECTIONS} \
         core errors; its interrupts moved, a blocked task re-homed, nothing ran or was drained \
         there; the refused writes refused; root brought it back; fifty times out and back \
         under load, the longest move holding its locks {hold} ns",
        mode.smp, mode.target
    );
    Ok(())
}

/// The lines `mode`'s boot must print, each with what its absence means,
/// the probes' first so that a probe let through is named as such.
fn wanted(mode: Mode) -> Vec<(String, String)> {
    let t = mode.target;
    let (without, all) = (mode.without_target(), mode.all());
    let mut wanted = vec![
        (
            format!("cpu: {t} out of service (by lode)"),
            format!("lode never took processor {t} out"),
        ),
        (
            format!("quarantine-test: cpu{t} online 0"),
            format!("cpu{t}/online did not read 0"),
        ),
        (
            format!("quarantine-test: online {without}"),
            format!("online did not leave processor {t} out"),
        ),
        (
            format!("quarantine-test: offline {t}"),
            format!("offline did not name processor {t}"),
        ),
        (
            format!("quarantine-test: nproc {}", mode.smp - 1),
            "nproc did not count one processor fewer".to_owned(),
        ),
        (
            format!("quarantine-test: tasks on {t}: 0"),
            format!("a task ran on processor {t}"),
        ),
        (
            format!("quarantine-test: pinned mask out {without}"),
            format!(
                "a task pinned to processor {t} did not read every processor in service while it was out"
            ),
        ),
        (
            "quarantine-test: pinned while out: elsewhere".to_owned(),
            format!("a task pinned to processor {t} did not run elsewhere while it was out"),
        ),
        (
            format!("cpu: {t} back in service (by root)"),
            format!("root's write did not bring processor {t} back"),
        ),
        (
            format!("quarantine-test: pinned mask back {t}"),
            format!("a task pinned to processor {t} did not read it alone after its return"),
        ),
        (
            "quarantine-test: pinned after the return: target".to_owned(),
            format!("a task pinned to processor {t} did not go back to it after its return"),
        ),
        (
            "quarantine-test: back 0".to_owned(),
            "root's write was refused".to_owned(),
        ),
        (
            format!("quarantine-test: online after {all}"),
            format!("online did not name processor {t} again"),
        ),
        (
            "quarantine-test: readers left: 0".to_owned(),
            "a reader ended by a signal never ran its end".to_owned(),
        ),
        (
            format!("quarantine-test: after probes {without}"),
            "a probe changed which processors are in service".to_owned(),
        ),
        (
            format!("quarantine-test: cycles 50 failures 0 loops alive 4 online {all}"),
            "fifty times out and back under load did not all succeed".to_owned(),
        ),
    ];
    if mode.probe().is_some() {
        wanted.insert(
            0,
            (
                "quarantine-probe: nonroot open=errno 13".to_owned(),
                "a user that is not root could open a processor's online file for writing"
                    .to_owned(),
            ),
        );
        wanted.insert(
            0,
            (
                "quarantine-probe: userns open=ok write=errno 1".to_owned(),
                "root of a user namespace of its own was not refused with EPERM".to_owned(),
            ),
        );
    }
    wanted
}

/// Whether `lines` show what the gate requires of `mode`'s boot.
fn judge(lines: &[String], mode: Mode) -> std::result::Result<(), String> {
    let has = |want: &str| lines.iter().any(|line| line.contains(want));
    let said = |what: &str| {
        lines.iter().find_map(|line| {
            line.split_once(what)
                .map(|(_, rest)| rest.trim().to_owned())
        })
    };
    if has(qemu::PANIC_MARKER) {
        return Err("the kernel stopped".to_owned());
    }
    if !has(DONE) {
        return Err("the guest never finished".to_owned());
    }
    let t = mode.target;
    let wanted = wanted(mode);
    for (want, why) in wanted {
        if !has(&want) {
            return Err(why);
        }
    }
    // The processor field read for some tasks at all: a stat without it
    // would say none ran there for the wrong reason.
    let read: u32 = said("quarantine-test: tasks read:")
        .and_then(|count| count.parse().ok())
        .unwrap_or(0);
    if read < 4 {
        return Err(format!(
            "/proc/<pid>/stat's processor field read for {read} tasks, fewer than the four loops"
        ));
    }
    let mce = said("quarantine-test: service ")
        .ok_or_else(|| format!("no /proc/ferrix-service line for cpu{t}"))?;
    if !mce.contains("service out drained 0") {
        return Err(format!(
            "a task was queued on processor {t} while it was out, and drained: {mce}"
        ));
    }
    rehomed(&mce, t)?;
    retargeted(lines, t)?;
    // lode's own count, not the kernel's: the take-out came after the
    // tenth core-scoped record, and none from before it.
    let out = format!("cpu: {t} out of service");
    let record = format!("mce: cpu {t} bank 1 corrected");
    let corrected = lines
        .iter()
        .skip_while(|line| !line.contains(READY))
        .take_while(|line| !line.contains(&out))
        .filter(|line| line.contains(&record))
        .count();
    if corrected < 10 {
        return Err(format!(
            "processor {t} went out after {corrected} corrected records, fewer than the count of 10"
        ));
    }
    Ok(())
}

/// A reader blocked on an empty pipe -- on no list -- homed on the target
/// `t` was moved at its wake (L.sched.103): the target's re-homed count. The
/// readers' homes said before the quarantine are information only: a
/// reader not yet blocked then may block on 2 after.
fn rehomed(mce: &str, t: usize) -> std::result::Result<(), String> {
    let rehomed: u32 = mce
        .split_once(" rehomed ")
        .and_then(|(_, count)| count.trim().parse().ok())
        .unwrap_or(0);
    if rehomed == 0 {
        return Err(format!(
            "no reader blocked on processor {t} was re-homed at its wake"
        ));
    }
    Ok(())
}

/// One snapshot of `/proc/interrupts` the guest said with `tag`: per
/// interrupt number, its count on each processor.
fn snapshot(lines: &[String], tag: &str) -> Vec<(u32, Vec<u64>)> {
    let prefix = format!("quarantine-test: {tag} ");
    lines
        .iter()
        .filter_map(|line| line.split_once(&prefix).map(|(_, rest)| rest))
        .filter_map(|rest| {
            let (irq, counts) = rest.trim().split_once(':')?;
            let counts: Vec<u64> = counts
                .split_whitespace()
                .map_while(|word| word.parse().ok())
                .collect();
            Some((irq.trim().parse().ok()?, counts))
        })
        .collect()
}

/// Processor 2's interrupts were moved: at least one, by the kernel's line;
/// none delivered there between the snapshot taken once it was out and the
/// one after the disk traffic; and the traffic reached one that had been
/// aimed there, now on the boot processor (L.iommu.101-103).
fn retargeted(lines: &[String], t: usize) -> std::result::Result<(), String> {
    let moved: u32 = lines
        .iter()
        .find_map(|line| line.split_once(&format!("cpu: {t}: ")))
        .and_then(|(_, rest)| rest.split_whitespace().next()?.parse().ok())
        .unwrap_or(0);
    if moved == 0 {
        return Err(format!("no interrupt was moved off processor {t}"));
    }
    let before = snapshot(lines, "before");
    let out = snapshot(lines, "out");
    let traffic = snapshot(lines, "traffic");
    let at = |snapshot: &[(u32, Vec<u64>)], irq: u32, cpu: usize| {
        snapshot
            .iter()
            .find(|(number, _)| *number == irq)
            .and_then(|(_, counts)| counts.get(cpu).copied())
            .unwrap_or(0)
    };
    for (irq, _) in &traffic {
        let (then, now) = (at(&out, *irq, t), at(&traffic, *irq, t));
        if now != then {
            return Err(format!(
                "a device interrupt landed on processor {t} while it was out: number {irq}, {then} then {now}"
            ));
        }
    }
    let reached = before
        .iter()
        .filter(|(irq, _)| at(&before, *irq, t) > 0)
        .any(|(irq, _)| at(&traffic, *irq, 0) > at(&out, *irq, 0));
    if !reached {
        return Err(format!(
            "the disk traffic reached no interrupt that had been aimed at processor {t}"
        ));
    }
    Ok(())
}

/// The guest half of the unremapped boot: root asks each secondary
/// processor out, then back, saying what came of each.
const UNREMAPPED_SCRIPT: &str = r#"
say() { echo "quarantine-test: $*"; }
cpu=/sys/devices/system/cpu
say ready
for c in 1 2 3; do
    echo 0 > $cpu/cpu$c/online
    say "cpu$c out rc $? online $(busybox cat $cpu/online)"
done
for c in 1 2 3; do echo 1 > $cpu/cpu$c/online; done
say "online after $(busybox cat $cpu/online)"
say done
"#;

/// Boot with VT-d's interrupt remapping off and devmgr told to start
/// drivers anyway (`ferrix.unisolated`), so devices' messages are in
/// compatibility format and aimed at the processors in turn: root's
/// take-out of one holding a message must be refused, the processor left
/// in service, and the native boot check's refusal arm runs
/// (`docs/CORE-QUARANTINE.md` §3.3, ledger 4992 (2)).
///
/// Verifies: L.iommu.104
fn test_unremapped(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if !args.arches()?.contains(&arch) {
        println!("  test-quarantine-unremapped: x86_64 only; skipped");
        return Ok(());
    }
    let files = image_files(arch, UNREMAPPED_SCRIPT.to_owned())?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = format!("{} ferrix.unisolated\n", init::command_line().trim_end());
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    let mut qemu_args = args.clone();
    qemu_args.unisolated_interrupts = true;
    qemu_args.smp = 4;
    qemu_args.smp_given = true;
    if !args.timeout_given {
        qemu_args.timeout = 300;
    }
    let lines = qemu::watch_lines(arch, &image, &kernel, &qemu_args, DONE)?;
    judge_unremapped(&lines).map_err(|why| {
        Error::new(format!(
            "{arch}: test-quarantine-unremapped: {why}.\n  Serial output is in {}",
            crate::paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!(
        "  {arch}: test-quarantine-unremapped: a processor holding a compatibility-format \
         message was refused and left in service"
    );
    Ok(())
}

/// At least one secondary refused for a message it holds, each such one
/// still online after its refusal, and every processor in service at the
/// end.
fn judge_unremapped(lines: &[String]) -> std::result::Result<(), String> {
    if lines.iter().any(|line| line.contains(qemu::PANIC_MARKER)) {
        return Err("the kernel stopped".to_owned());
    }
    if !lines.iter().any(|line| line.contains(DONE)) {
        return Err("the guest never finished".to_owned());
    }
    let refused: Vec<usize> = (1..4)
        .filter(|cpu| {
            let line = format!(
                "cpu: {cpu} stays in service (by root): an interrupt aimed at it cannot be moved"
            );
            lines.iter().any(|seen| seen.contains(&line))
        })
        .collect();
    if refused.is_empty() {
        return Err("no processor was refused for a message it holds".to_owned());
    }
    for cpu in &refused {
        let said = format!("quarantine-test: cpu{cpu} out rc ");
        let line = lines
            .iter()
            .find_map(|line| line.split_once(&said).map(|(_, rest)| rest.to_owned()))
            .ok_or_else(|| format!("no line for processor {cpu}'s take-out"))?;
        let online = line
            .split_once(" online ")
            .map_or("", |(_, list)| list.trim());
        let listed = online.split(',').any(|part| match part.split_once('-') {
            Some((first, last)) => {
                first.parse::<usize>().is_ok_and(|first| first <= *cpu)
                    && last.parse::<usize>().is_ok_and(|last| *cpu <= last)
            }
            None => part.parse() == Ok(*cpu),
        });
        if line.starts_with("0 ") || !listed {
            return Err(format!(
                "processor {cpu} was refused and yet went out of service: {line}"
            ));
        }
    }
    if !lines
        .iter()
        .any(|line| line.contains("quarantine-test: online after 0-3"))
    {
        return Err("a processor was left out of service at the end".to_owned());
    }
    Ok(())
}
