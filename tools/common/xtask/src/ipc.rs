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
    let (boots, reference) = if !args.board_logs.is_empty() {
        (board_boots(&args)?, Reference::None)
    } else if let Some(reference) = args.alternate.clone() {
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
    let fingerprint = match args.board.as_deref() {
        Some(board) if !args.board_logs.is_empty() => board_fingerprint(board, &boots)?,
        _ => crate::hotpath::Fingerprint::of_this_host(arch, args)?,
    };
    let root = crate::paths::workspace_root();
    let rounds = if matches!(reference, Reference::None) {
        1
    } else {
        rounds
    };
    let mut configuration = record::ipc_configuration(args, rounds, args.alternate.as_deref());
    if let (Some(board), Some(processors)) = (args.board.as_deref(), board_processors(&boots)) {
        record::set_board(&mut configuration, board, processors);
    }
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

/// `--board-log`: each log one boot of a board that ran `/sbin/ipc-bench`
/// at its shell, as `run_once` reads a QEMU boot. A log whose run did not
/// finish (no `ipc-bench domain-call` line) is refused, not skipped.
fn board_boots(args: &Args) -> Result<Vec<Boot>> {
    if args.board.is_none() {
        return Err(Error::new(
            "--board-log needs --board, the board the logs are from",
        ));
    }
    let mut boots = Vec::new();
    for log in &args.board_logs {
        let text = std::fs::read_to_string(log)
            .map_err(|error| Error::new(format!("reading {log}: {error}")))?;
        let lines: Vec<String> = text.lines().map(|line| line.trim().to_owned()).collect();
        if field(&lines, "domain-call", "p50").is_none() {
            return Err(Error::new(format!(
                "{log}: no `ipc-bench domain-call` line"
            )));
        }
        for line in lines.iter().filter(|line| line.contains("ipc-bench")) {
            println!("  {log}: {line}");
        }
        boots.push(Boot { lines });
    }
    Ok(boots)
}

/// The processors a board's boots ran on, from the kernel's `cpus` line
/// (`N described by firmware, M online`), the same in every boot or `None`.
fn board_processors(boots: &[Boot]) -> Option<u64> {
    let of = |boot: &Boot| {
        boot.lines.iter().find_map(|line| {
            let (_, rest) = line.split_once("described by firmware, ")?;
            rest.split_whitespace().next()?.parse::<u64>().ok()
        })
    };
    let first = of(boots.first()?)?;
    boots
        .iter()
        .all(|boot| of(boot) == Some(first))
        .then_some(first)
}

/// A board's fingerprint: its name, its processor count and the counter's
/// rate as the kernel read them, and no virtual machine. The board is
/// named, not probed: nothing runs on it but the image.
fn board_fingerprint(board: &str, boots: &[Boot]) -> Result<crate::hotpath::Fingerprint> {
    use crate::hotpath::json::Value;
    let clock = boots
        .first()
        .and_then(|boot| {
            boot.lines.iter().find_map(|line| {
                let (_, rest) = line.split_once("clock    generic timer at ")?;
                Some(rest.trim().to_owned())
            })
        })
        .ok_or_else(|| Error::new("the board's log has no `clock    generic timer at` line"))?;
    let processors = board_processors(boots)
        .ok_or_else(|| Error::new("the board's logs do not agree on a processor count"))?;
    let host = Value::object([
        ("board", Value::str(board)),
        ("counter", Value::str(clock)),
        (
            "topology",
            Value::object([("processors", Value::int(processors))]),
        ),
    ]);
    let vm = Value::object([("accelerator", Value::str("none: the board itself"))]);
    Ok(crate::hotpath::Fingerprint::new(host, vm))
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
        // `--kernel-option ferrix.fastpath=on` measures the fast path
        // (`docs/OPAQUE-KERNEL.md` §9.7); the counts line says which path the
        // trips took.
        let cmdline = crate::image_cmdline(args);
        let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, cmdline.as_deref())?;
        let host = Host::before(args.pin.as_deref());
        let lines = qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)?;
        let host = format!("  {arch}: {}", host.after());
        println!("{host}");
        all.push(host);
        let mut finished = false;
        for line in &lines {
            if line.contains("ipc-bench") || line.contains("fastpath") {
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
    let _ = command
        .current_dir(tree)
        .env("CARGO_TARGET_DIR", tree.join("target"))
        .args(other_tree_args(args));
    let output = command
        .output()
        .map_err(|error| Error::new(format!("could not run {reference}'s bench-ipc: {error}")))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<String> = text.lines().map(|line| line.trim().to_owned()).collect();
    let finished = lines
        .iter()
        .any(|line| line.contains("ipc-bench") && line.contains("exit 0"));
    if !output.status.success() || !finished {
        // The other tree's own words are the only account of why it stopped.
        let errors = String::from_utf8_lossy(&output.stderr);
        let tail = |text: &str| {
            let count = text.lines().count();
            text.lines()
                .skip(count.saturating_sub(40))
                .collect::<Vec<_>>()
                .join("\n")
        };
        return Err(Error::new(format!(
            "{reference}'s bench-ipc did not finish ({})\n--- its stdout, last lines ---\n{}\n--- its stderr, last lines ---\n{}",
            output.status,
            tail(&text),
            tail(&errors)
        )));
    }
    Ok(Boot { lines })
}

/// The other tree's `bench-ipc` arguments: this run's, so both sides boot the
/// same configuration. The kernel options go too, or `ferrix.fastpath=on`
/// would be measured against the general path.
fn other_tree_args(args: &Args) -> Vec<String> {
    let mut out: Vec<String> = ["xtask", "bench-ipc", "--arch", "x86_64", "--smp", "1"]
        .map(String::from)
        .to_vec();
    if let Some(init) = args.init.as_deref() {
        out.extend(["--init".to_owned(), init.to_owned()]);
    }
    if args.release {
        out.push("--release".to_owned());
    }
    if let Some(accel) = args.accel.as_deref() {
        out.extend(["--accel".to_owned(), accel.to_owned()]);
    }
    for option in &args.kernel_options {
        out.extend(["--kernel-option".to_owned(), option.clone()]);
    }
    out
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

/// The line the kernel prints its fast path counts on as the shell exits
/// (`fastpath::report_counts`), trips first.
const COUNTS_LINE: &str = "fastpath counts:";

/// What the shell runs for `test-ipc-equiv`.
const EQUIV_SCRIPT: &str = r#"/sbin/ipc-equiv
"#;

/// The stage-9 line that says which path `channel_write_read` takes
/// (`ferrix.fastpath`, `src/kernel/src/fastpath.rs`).
const FASTPATH_LINE: &str = "ipc fast path for channel_write_read:";

/// `test-ipc-equiv`: boot `/sbin/ipc-equiv` (`src/user/system/native/ipc-equiv`)
/// with `ferrix.fastpath=off` on every architecture asked for, and on x86-64
/// again with `ferrix.fastpath=on`, the one architecture the fast path is for
/// (`docs/OPAQUE-KERNEL.md` §9.7, part 6). Each boot must print the stage-9
/// line naming the path it was asked for and every case's line, end with
/// `ipc-equiv: exit 0`, and the two x86-64 transcripts must be the same line
/// for line.
///
/// The kernel's counts of the fast path, printed as the shell exits, must
/// show trips taken on the boot with it on and nothing at all moved on a
/// boot with it off: a comparison of two general paths would pass as well.
///
/// # Errors
///
/// No `--init`; a build or boot that fails; a boot whose stage-9 line names
/// the wrong path; a transcript that did not finish; two that differ.
/// Verifies: `L.x86_64.150`, `L.x86_64.159`, `L.x86_64.160`, `L.sched.59`
/// Verifies: `L.sched.61`, `L.object.167`, `L.object.170`, `H.OBJ.18`
/// Verifies: `L.x86_64.161`
pub(crate) fn test_ipc_equiv(args: &Args) -> Result<()> {
    let init = args.init.as_deref().ok_or_else(|| {
        Error::new("test-ipc-equiv needs --init, a static busybox for each architecture")
    })?;
    for arch in args.arches()? {
        let program = crate::program_for(init, arch)?;
        let loader = cargo::build_loader(arch, args.release)?;
        let kernel = cargo::build_kernel_with_init(arch, args.release, &program, EQUIV_SCRIPT)?;
        let natives = native::build(arch, args.release)?;
        let carried = shell::carried_for(arch, &program, args)?;
        let initramfs = initramfs::build(None, &natives, None, &carried)?;
        let settings: &[&str] = if arch == crate::paths::Arch::X86_64 {
            &["off", "on"]
        } else {
            &["off"]
        };
        let mut transcripts = Vec::new();
        for setting in settings {
            let cmdline = format!("ferrix.fastpath={setting}\n");
            let image = fat::write_image_with(arch, &loader, &kernel, &initramfs, Some(&cmdline))?;
            let lines = qemu::watch_lines(arch, &image, &kernel, args, shell::EXITED)?;
            let said = lines
                .iter()
                .find(|line| line.contains(FASTPATH_LINE))
                .ok_or_else(|| Error::new(format!("{arch}: no stage-9 line for the fast path")))?;
            if !said.contains(&format!(
                "{FASTPATH_LINE} {setting} (as ferrix.fastpath asked)"
            )) {
                return Err(Error::new(format!(
                    "{arch}: asked for ferrix.fastpath={setting}, the boot said: {}",
                    said.trim()
                )));
            }
            let transcript: Vec<String> = lines
                .iter()
                .filter_map(|line| {
                    line.get(line.find("ipc-equiv")?..)
                        .map(|rest| rest.trim().to_owned())
                })
                .collect();
            for line in &transcript {
                println!("  {arch} fastpath={setting}: {line}");
            }
            let counts = lines
                .iter()
                .find_map(|line| line.get(line.find(COUNTS_LINE)?..))
                .ok_or_else(|| Error::new(format!("{arch}: no fast path counts line")))?
                .trim()
                .to_owned();
            println!("  {arch} fastpath={setting}: {counts}");
            let numbers: Vec<u64> = counts
                .split_whitespace()
                .filter_map(|word| word.split_once('=')?.1.parse().ok())
                .collect();
            let trips = numbers.first().copied().unwrap_or(0);
            if *setting == "on" && trips == 0 {
                return Err(Error::new(format!(
                    "{arch}: ferrix.fastpath=on took no trip through the fast path"
                )));
            }
            if *setting == "off" && numbers.iter().any(|&count| count != 0) {
                return Err(Error::new(format!(
                    "{arch}: a fast path counter moved with ferrix.fastpath=off: {counts}"
                )));
            }
            if !transcript.iter().any(|line| line == "ipc-equiv: exit 0") {
                return Err(Error::new(format!(
                    "{arch}: ipc-equiv did not finish with ferrix.fastpath={setting}"
                )));
            }
            transcripts.push(transcript);
        }
        if let [off, on] = transcripts.as_slice()
            && off != on
        {
            let first = off
                .iter()
                .zip(on)
                .find(|(off, on)| off != on)
                .map_or_else(String::new, |(off, on)| {
                    format!("off said {off:?}, on said {on:?}")
                });
            return Err(Error::new(format!(
                "{arch}: the transcripts with the fast path off and on differ: {first}"
            )));
        }
        if transcripts.len() == 2 {
            println!("  {arch}: the transcripts with the fast path off and on are the same");
        }
    }
    Ok(())
}

#[cfg(test)]
mod other_tree_tests {
    use super::other_tree_args;
    use crate::args::Args;

    fn args(line: &[&str]) -> Args {
        Args::parse(line.iter().map(|word| (*word).to_owned())).unwrap()
    }

    #[test]
    fn the_other_tree_boots_with_the_same_kernel_options() {
        let line = args(&[
            "bench-ipc",
            "--release",
            "--accel",
            "kvm",
            "--init",
            "/bb/{arch}/busybox",
            "--kernel-option",
            "ferrix.fastpath=on",
            "--alternate",
            "main",
        ]);
        let out = other_tree_args(&line);
        let at = out
            .iter()
            .position(|word| word == "--kernel-option")
            .unwrap();
        assert_eq!(out[at + 1], "ferrix.fastpath=on");
        assert!(out.contains(&"--release".to_owned()));
        assert!(!out.contains(&"--alternate".to_owned()));
    }

    #[test]
    fn no_init_passes_no_empty_init() {
        let out = other_tree_args(&args(&["bench-ipc", "--alternate", "main"]));
        assert!(!out.contains(&"--init".to_owned()));
        assert!(!out.iter().any(String::is_empty));
    }
}

#[cfg(test)]
mod board_tests {
    use super::{board_fingerprint, board_processors};
    use crate::args::Args;
    use crate::hotpath::record::Boot;

    fn boot(processors: u64, p50: u64) -> Boot {
        let lines = [
            "19:20:01.100     0.40 |   clock    generic timer at 24.000 MHz".to_owned(),
            format!(
                "19:20:01.200     0.90 |   cpus     {processors} described by firmware, {processors} online, booted on 0 0x0"
            ),
            format!(
                "19:20:09.000 ipc-bench domain-call n=20000 min=1 p50={p50} p90=1 p99=1 mean=1"
            ),
        ];
        Boot {
            lines: lines.to_vec(),
        }
    }

    #[test]
    fn the_board_flags_parse() {
        let args = Args::parse(
            [
                "bench-ipc",
                "--board",
                "dk1",
                "--board-log",
                "a.log",
                "--board-log",
                "b.log",
            ]
            .iter()
            .map(|word| (*word).to_owned()),
        )
        .unwrap();
        assert_eq!(args.board.as_deref(), Some("dk1"));
        assert_eq!(args.board_logs, ["a.log", "b.log"]);
    }

    #[test]
    fn a_board_is_named_by_its_logs_and_not_by_this_host() {
        let boots = [boot(2, 9000), boot(2, 9100)];
        assert_eq!(board_processors(&boots), Some(2));
        let one = board_fingerprint("stm32mp157d-dk1", &boots).unwrap();
        let again = board_fingerprint("stm32mp157d-dk1", &boots[1..]).unwrap();
        assert_eq!(
            one.hw_hash, again.hw_hash,
            "the figures must not move the hash"
        );
        let other = board_fingerprint("stm32mp157d-dk1", &[boot(1, 9000)]).unwrap();
        assert_ne!(one.hw_hash, other.hw_hash, "the processor count must");
    }

    #[test]
    fn boots_that_disagree_on_processors_have_no_count() {
        assert_eq!(board_processors(&[boot(2, 1), boot(1, 1)]), None);
        assert!(board_fingerprint("dk1", &[boot(2, 1), boot(1, 1)]).is_err());
    }
}
