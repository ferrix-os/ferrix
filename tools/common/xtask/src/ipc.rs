//! `bench-ipc`: a channel round trip between two native processes, timed.
//!
//! `docs/OPAQUE-KERNEL.md`'s trip is a disk read through the block ring and a
//! ring-3 driver, device and all. The figure an IPC design is compared by --
//! seL4's, Zircon's -- is narrower: a message to another process and back,
//! nothing else. This boots `--init`'s shell with a script that runs
//! `/sbin/ipc-bench`
//! (`src/user/system/native/ipc-bench`), which starts its own echo server in
//! a cgroup of its own and prints the floor (a native call that does not
//! sleep) and the trip, in nanoseconds.

use crate::args::Args;
use crate::hotpath::record::{self, Boot, Measurement, Reference};
use crate::{Error, Result, cargo, fat, initramfs, native, qemu, shell};

/// What the shell runs.
const SCRIPT: &str = r#"/sbin/ipc-bench
echo "ipc-bench: exit $?"
"#;

/// The host processor `bench-ipc` pins QEMU to unless `--pin` says
/// otherwise: the one the seL4 and Redox runs were measured on, so the
/// figures compare (`docs/OPAQUE-KERNEL.md` §9.6).
const PIN: &str = "11";

/// Boot, run the benchmark, and print its lines; with `--alternate`,
/// `--against-sel4` or `--against-redox`, take turns with the other side
/// `--rounds` times and print the ratio. With `--record`, file the result
/// under `docs/hotpaths/results/ipc-round-trip/` (`docs/HOTPATHS.md` §6).
///
/// One processor unless `--smp` says otherwise, and QEMU pinned to host
/// processor [`PIN`] unless `--pin` names others or `none`; each run prints
/// the host's load and how busy the pinned processor's SMT sibling was, which
/// is what moves a figure most on a shared host.
///
/// # Errors
///
/// No `--init`, a build or boot that fails, or a benchmark that did not
/// finish.
pub(crate) fn bench_ipc(args: &Args) -> Result<()> {
    let mut args = args.clone();
    if !args.smp_given {
        args.smp = 1;
    }
    if args.pin.is_none() && host_processors() > 12 {
        args.pin = Some(PIN.to_owned());
    }
    let rounds = args.rounds.unwrap_or(3);
    let (boots, reference) = if let Some(reference) = args.alternate.clone() {
        alternate(&args, &reference, rounds)?
    } else if args.against_sel4 || args.against_redox {
        against(&args, rounds)?
    } else {
        (vec![run_once(&args)?], Reference::None)
    };
    if args.record {
        write_record(&args, rounds, boots, reference)?;
    }
    Ok(())
}

/// The hot path `bench-ipc` measures, its line prefix and its figure.
const HOT_PATH: (&str, &str, &str) = ("ipc-round-trip", "ipc-bench", "domain-call");

/// `--record`: the fingerprint of this host and of the x86-64 guest, and
/// the record, written and named. The log is `FERRIX_HOTPATH_LOG` if set:
/// `bench-ipc` does not know where its own output is kept.
fn write_record(args: &Args, rounds: u32, boots: Vec<Boot>, reference: Reference) -> Result<()> {
    let arch = args
        .arches()?
        .into_iter()
        .next()
        .unwrap_or(crate::paths::Arch::X86_64);
    let fingerprint = crate::hotpath::Fingerprint::of_this_host(arch, args)?;
    let root = crate::paths::workspace_root();
    let rounds = if matches!(reference, Reference::None) {
        1
    } else {
        rounds
    };
    let configuration = record::ipc_configuration(args, rounds, args.alternate.as_deref());
    let home = std::env::var("HOME").unwrap_or_default();
    let log = std::env::var("FERRIX_HOTPATH_LOG")
        .ok()
        .map(|log| record::without_home(&log, &home));
    let measurement = Measurement {
        path: HOT_PATH.0,
        prefix: HOT_PATH.1,
        figure: HOT_PATH.2,
        boots,
        reference,
    };
    let value = record::value(
        &measurement,
        &fingerprint,
        configuration,
        record::ferrix_commit(&root)?,
        &record::utc_now(),
        log.as_deref(),
    );
    let file = record::write(&root, &value)?;
    println!(
        "  bench-ipc --record: {} (hardware {})",
        file.strip_prefix(&root).unwrap_or(&file).display(),
        fingerprint.hw_hash
    );
    Ok(())
}

/// One boot of this tree's benchmark, its lines printed and answered.
fn run_once(args: &Args) -> Result<Boot> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new("bench-ipc needs --init, a static busybox for each architecture")
    })?;
    let mut all = Vec::new();
    for arch in args.arches()? {
        let program = crate::program_for(init, arch)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        let carried = shell::carried_for(arch, &program, args)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, None)?;
        let host = Host::before(args.pin.as_deref());
        let lines = qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)?;
        let host = format!("  {arch}: {}", host.after());
        println!("{host}");
        all.push(host);
        let mut finished = false;
        for line in &lines {
            if line.contains("ipc-bench") {
                println!("  {arch}: {}", line.trim());
                finished |= line.contains("ipc-bench: exit 0");
            }
            all.push(line.trim().to_owned());
        }
        if !finished {
            return Err(Error::new(format!("{arch}: ipc-bench did not finish")));
        }
    }
    Ok(Boot { lines: all })
}

/// The value of `key=` in the first of `lines` that holds `what`, as a
/// number.
fn field(lines: &[String], what: &str, key: &str) -> Option<u64> {
    lines
        .iter()
        .filter(|line| line.contains(what))
        .find_map(|line| {
            line.split_whitespace()
                .find_map(|word| word.strip_prefix(key)?.strip_prefix('=')?.parse().ok())
        })
}

/// A figure for a line, or `?`.
fn shown(figure: Option<u64>) -> String {
    figure.map_or_else(|| "?".to_owned(), |ns| ns.to_string())
}

/// `--alternate`: build `reference` in a tree of its own, and run the two
/// benchmarks turn about `rounds` times, this tree first. The other tree's
/// xtask may know no `--pin`, so it runs under `taskset` itself, after one
/// unpinned run that builds it and is not counted.
fn alternate(args: &Args, reference: &str, rounds: u32) -> Result<(Vec<Boot>, Reference)> {
    let root = crate::paths::workspace_root();
    let sha = git_output(&root, &["rev-parse", "--short=12", reference])?;
    let tree = root
        .join(".claude/worktrees")
        .join(format!("bench-alt-{sha}"));
    if !tree.is_dir() {
        let _ = git_output(
            &root,
            &["worktree", "add", "--detach", &tree.to_string_lossy(), &sha],
        )?;
    }
    println!("  bench-ipc --alternate {reference} ({sha}): building it, one run not counted");
    let _ = other_tree(args, &tree, reference, false)?;
    let mut pairs = Vec::new();
    let (mut boots, mut others) = (Vec::new(), Vec::new());
    for round in 1..=rounds {
        let mine = run_once(args)?.lines;
        let theirs = other_tree(args, &tree, reference, true)?.lines;
        let here = field(&mine, "domain-call", "p50");
        let there = field(&theirs, "domain-call", "p50");
        println!(
            "  round {round}: domain-call p50 {} ns here, {} ns at {reference}; call p50 {} \
             here, {} there",
            shown(here),
            shown(there),
            shown(field(&mine, " call ", "p50")),
            shown(field(&theirs, " call ", "p50")),
        );
        if let (Some(here), Some(there)) = (here, there) {
            pairs.push((here, there));
        }
        boots.push(Boot { lines: mine });
        others.push(Boot { lines: theirs });
    }
    ratio("here", reference, &pairs);
    let commit = git_output(&root, &["rev-parse", &sha])?;
    let reference = Reference::Tree {
        name: reference.to_owned(),
        commit,
        boots: others,
    };
    Ok((boots, reference))
}

/// One run of `tree`'s own `bench-ipc`, pinned by `taskset` when `pinned`:
/// every line it printed.
fn other_tree(args: &Args, tree: &std::path::Path, reference: &str, pinned: bool) -> Result<Boot> {
    let mut command = match args.pin.as_deref().filter(|_| pinned) {
        Some(cpus) => {
            let mut taskset = std::process::Command::new("taskset");
            let _ = taskset.args(["-c", cpus, "cargo"]);
            taskset
        }
        None => std::process::Command::new("cargo"),
    };
    let init = args.init.clone().unwrap_or_default();
    let _ = command
        .current_dir(tree)
        .env("CARGO_TARGET_DIR", tree.join("target"))
        .args([
            "xtask",
            "bench-ipc",
            "--arch",
            "x86_64",
            "--smp",
            "1",
            "--init",
            &init,
        ]);
    if args.release {
        let _ = command.arg("--release");
    }
    if let Some(accel) = args.accel.as_deref() {
        let _ = command.args(["--accel", accel]);
    }
    let output = command
        .output()
        .map_err(|error| Error::new(format!("could not run {reference}'s bench-ipc: {error}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = text.lines().map(|line| line.trim().to_owned()).collect();
    let finished = lines
        .iter()
        .any(|line| line.contains("ipc-bench") && line.contains("exit 0"));
    if !output.status.success() || !finished {
        return Err(Error::new(format!(
            "{reference}'s bench-ipc did not finish"
        )));
    }
    Ok(Boot { lines })
}

/// `--against-sel4` and `--against-redox`: this tree's domain-call against
/// the other kernel's own round trip, turn about. Each runner is the one its
/// figure was measured with, under the gate's CPU model on host processor
/// [`PIN`]: `run.sh` in `~/.local/share/ferrix/sel4`, `run.py` in
/// `~/.local/share/ferrix/redox-bench`. What is missing says so.
fn against(args: &Args, rounds: u32) -> Result<(Vec<Boot>, Reference)> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let share = home.join(".local/share/ferrix");
    let other = if args.against_sel4 { "seL4" } else { "Redox" };
    if !args.against_sel4 {
        let _ = redox_round_trip(&share.join("redox-bench"))?;
    }
    let mut pairs = Vec::new();
    let (mut boots, mut figures) = (Vec::new(), Vec::new());
    for round in 1..=rounds {
        let mine = run_once(args)?.lines;
        let theirs = sel4_round_trip(&share.join("sel4"), round)?;
        let here = field(&mine, "domain-call", "p50");
        println!(
            "  round {round}: domain-call p50 {} ns here, {other} {theirs} ns",
            shown(here)
        );
        if let Some(here) = here {
            pairs.push((here, theirs));
        }
        boots.push(Boot { lines: mine });
        figures.push(theirs);
    }
    ratio("Ferrix", other, &pairs);
    let reference = Reference::Kernel {
        name: "seL4 matched-nopcid (run.sh)".to_owned(),
        p50_ns: figures,
    };
    Ok((boots, reference))
}

/// seL4's `Call`/`ReplyRecv` round trip, p50 in nanoseconds, the median of
/// one image boot's runs: `run.sh matched-nopcid nopcid`, whose root task
/// times as `ipc-bench` does (`sel4bench-ferrix-rt.patch`).
fn sel4_round_trip(dir: &std::path::Path, round: u32) -> Result<u64> {
    let runner = dir.join("run.sh");
    if !runner.is_file() || !dir.join("build/matched-nopcid").is_dir() {
        return Err(Error::new(format!(
            "--against-sel4 needs {} and its build matched-nopcid (fetch.sh, build.sh)",
            runner.display()
        )));
    }
    let tag = format!("bench-ipc-r{round}");
    let output = std::process::Command::new(&runner)
        .args(["matched-nopcid", "nopcid", &tag, PIN])
        .output()
        .map_err(|error| Error::new(format!("could not run {}: {error}", runner.display())))?;
    let lines: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    let mhz = field(&lines, "tsc_mhz", "tsc_mhz").filter(|&mhz| mhz > 0);
    let mut p50s: Vec<u64> = lines
        .iter()
        .filter(|line| line.contains("call-replyrecv-roundtrip"))
        .filter_map(|line| field(core::slice::from_ref(line), "roundtrip", "p50"))
        .collect();
    p50s.sort_unstable();
    match (mhz, p50s.get(p50s.len() / 2)) {
        (Some(mhz), Some(&cycles)) => Ok(cycles * 1000 / mhz),
        _ => Err(Error::new(format!(
            "seL4's run {tag} printed no round trip"
        ))),
    }
}

/// Why `--against-redox` has nothing to compare with yet: the Redox runner
/// times disk reads and a pipe's ping-pong, not a message to another process
/// and back by Redox's own IPC, so no figure of the same call stands beside
/// ours.
fn redox_round_trip(dir: &std::path::Path) -> Result<u64> {
    Err(Error::new(format!(
        "--against-redox is a stub: {} times disk reads and an 8-byte pipe ping-pong \
         (RB name=pipe-pingpong-8B), not an IPC round trip like ipc-bench's; it needs an \
         rbench case that sends a message to another process and back by Redox's own \
         IPC first",
        dir.join("run.py").display()
    )))
}

/// The ratio of each pair, here over there: its median and its spread.
fn ratio(here: &str, there: &str, pairs: &[(u64, u64)]) {
    let mut ratios: Vec<f64> = pairs
        .iter()
        .filter(|(_, there)| *there > 0)
        .map(|&(here, there)| here as f64 / there as f64)
        .collect();
    ratios.sort_by(f64::total_cmp);
    let (Some(least), Some(most)) = (ratios.first(), ratios.last()) else {
        println!("  no round gave both figures");
        return;
    };
    let median = ratios.get(ratios.len() / 2).copied().unwrap_or(0.0);
    println!(
        "  {here} over {there}, domain-call p50: {median:.3}, the median of {} rounds, spread \
         {least:.3} to {most:.3}",
        ratios.len()
    );
}

/// How many processors the host has, from `/proc/cpuinfo`.
fn host_processors() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|text| {
            text.lines()
                .filter(|line| line.starts_with("processor"))
                .count()
        })
        .unwrap_or(0)
}

/// The host as a run began: its load and, when QEMU is pinned to one
/// processor, that processor's SMT sibling's busy and total ticks.
struct Host {
    /// The one-minute load at the start.
    load: String,
    /// The sibling, and its busy and total ticks at the start.
    sibling: Option<(usize, u64, u64)>,
}

impl Host {
    /// Read now.
    fn before(pin: Option<&str>) -> Host {
        let sibling = pin
            .and_then(|cpus| cpus.parse::<usize>().ok())
            .and_then(sibling_of)
            .and_then(|cpu| ticks(cpu).map(|(busy, total)| (cpu, busy, total)));
        Host {
            load: load(),
            sibling,
        }
    }

    /// The run's host line: the load before and after, and the sibling's
    /// busy share over the run.
    fn after(&self) -> String {
        let sibling = self.sibling.and_then(|(cpu, busy, total)| {
            let (busy_now, total_now) = ticks(cpu)?;
            let share =
                busy_now.saturating_sub(busy) * 100 / total_now.saturating_sub(total).max(1);
            Some(format!(", SMT sibling cpu{cpu} {share}% busy"))
        });
        format!(
            "host load {} before, {} after{}",
            self.load,
            load(),
            sibling.unwrap_or_default()
        )
    }
}

/// The one-minute load average.
fn load() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_owned))
        .unwrap_or_else(|| "?".to_owned())
}

/// The other processor of `cpu`'s core, if it has one.
fn sibling_of(cpu: usize) -> Option<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    let text = std::fs::read_to_string(path).ok()?;
    text.trim()
        .split([',', '-'])
        .filter_map(|word| word.parse().ok())
        .find(|&other| other != cpu)
}

/// `cpu`'s busy and total ticks from `/proc/stat`.
fn ticks(cpu: usize) -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let name = format!("cpu{cpu}");
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(name.as_str()))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .take(7)
        .filter_map(|word| word.parse().ok())
        .collect();
    let [user, nice, system, idle, iowait, irq, softirq] = values.as_slice() else {
        return None;
    };
    let busy = user + nice + system + irq + softirq;
    Some((busy, busy + idle + iowait))
}

/// `git` with `arguments` in `root`, its output trimmed.
fn git_output(root: &std::path::Path, arguments: &[&str]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| Error::new(format!("could not run git: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "git {}: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
