//! `check`'s test binaries, run side by side with what follows them
//! (`docs/TEST-TIME.md`, *The main workspace: the plan*, P1; the
//! certification consultant's design review, ledger line 1594).
//!
//! `cargo test` runs each test binary after the last, and nothing else in
//! `check` starts until the last has ended: 33 to 68 s in the gate, most of
//! it two btrfs crates. cargo 1.97 does not hold its build lock while test
//! binaries run, so `check` can build the doc tests, the documentation and
//! the cross-target clippies meanwhile, and the binaries can run beside each
//! other.
//!
//! cargo is given this program as the tests' runner
//! (`target.'cfg(all())'.runner`). For each binary cargo calls
//! `xtask test-runner <binary> <arguments>` with the environment and the
//! directory it gives the binary itself; the runner writes the command down,
//! starts `xtask test-wait` in the background, and returns success at once.
//! `test-wait` takes one of [`AT_ONCE`] slots, takes the SHA-256 of the
//! binary and of each `CARGO_BIN_EXE_*` it is given, runs the binary with
//! cargo's arguments, environment (less this module's variables) and
//! directory, its output to a file, and writes how it ended beside it. If the
//! `check` that started it is gone, it kills the binary and says so.
//!
//! [`Launched::finish`] waits for every binary, prints each one's output
//! whole with its path, exit status and time, and fails closed: on a binary
//! that failed, died of a signal, could not start, never reported, or whose
//! files changed while it ran; on a "Running" line of cargo's without its
//! record, or a record without its line; and on no binary at all.
//!
//! What changes against `cargo test` alone: binaries run at the same time,
//! as one binary's tests already do; every binary runs to its end, where
//! cargo stopped at the first that failed; and the output comes after the
//! steps that ran meanwhile. `FERRIX_TESTS_IN_TURN=1` runs them as cargo
//! does; release runs and the baseline use it.

use std::collections::BTreeSet;
use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result, sha256};

/// The variable that names the directory a run's records go to.
const VARIABLE: &str = "FERRIX_TEST_RUNS";

/// The variable that names the `check` process the binaries belong to.
const OWNER: &str = "FERRIX_TEST_RUNS_OWNER";

/// The variable that keeps cargo's own way: one binary after another.
pub(crate) const IN_TURN: &str = "FERRIX_TESTS_IN_TURN";

/// Binaries running at once.
pub(crate) const AT_ONCE: usize = 6;

/// How long a binary waits for a slot before it fails: longer than any
/// binary runs, so only a slot that is never given back reaches it.
const SLOT_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Whether test binaries run side by side: on Unix, unless [`IN_TURN`]
/// says otherwise.
pub(crate) fn side_by_side() -> bool {
    cfg!(unix) && std::env::var_os(IN_TURN).is_none_or(|value| value.is_empty() || value == "0")
}

/// `cargo test <arguments>` with this program as its runner, started in
/// `root`; the binaries left running and recorded in `directory`, which is
/// made anew.
///
/// # Errors
///
/// cargo failing (a build error); whatever started is waited for first.
pub(crate) fn launch(
    cargo: &str,
    root: &Path,
    arguments: &[String],
    directory: &Path,
) -> Result<Launched> {
    let _ = std::fs::remove_dir_all(directory);
    std::fs::create_dir_all(directory)?;
    let exe = std::env::current_exe()?;
    let exe = exe
        .to_str()
        .ok_or_else(|| Error::new("xtask's path is not UTF-8"))?
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    println!(
        "   the test binaries run side by side, {AT_ONCE} at once ({IN_TURN}=1 runs them in turn)"
    );
    let mut child = Command::new(cargo)
        .current_dir(root)
        .args(arguments)
        .arg("--config")
        .arg(format!(
            "target.'cfg(all())'.runner=[\"{exe}\", \"test-runner\"]"
        ))
        .env(VARIABLE, directory)
        .env(OWNER, std::process::id().to_string())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Error::new(format!("could not run cargo test: {error}")))?;
    // cargo's own lines, passed on as they come, and the binaries its
    // "Running" lines name.
    let mut running = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        for line in BufReader::new(stderr).lines() {
            let line = line?;
            eprintln!("{line}");
            if let Some(binary) = running_binary(&line) {
                running.push(binary);
            }
        }
    }
    let status = child.wait()?;
    let launched = Launched {
        directory: directory.to_path_buf(),
        began: Instant::now(),
        running,
    };
    if !status.success() {
        // Whatever started is waited for, so nothing is left running.
        let _ = launched.finish();
        return Err(Error::new(format!("cargo test failed ({status})")));
    }
    Ok(launched)
}

/// The binary a line of cargo's names, when it says it runs one:
/// `     Running unittests src/lib.rs (<path>)`.
pub(crate) fn running_binary(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("Running ")?;
    let (_, path) = rest.rsplit_once(" (")?;
    Some(path.strip_suffix(')')?.to_owned())
}

/// The test binaries a `cargo test` started through the runner, running.
#[derive(Debug)]
pub(crate) struct Launched {
    /// Where their records are.
    directory: PathBuf,
    /// When cargo returned.
    began: Instant,
    /// The binaries cargo's "Running" lines named, in order.
    running: Vec<String>,
}

/// How one binary ended, as its records say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ended {
    /// The command, binary first.
    pub(crate) command: Vec<String>,
    /// Its status: an exit code, or what went wrong.
    pub(crate) status: String,
    /// What it printed.
    pub(crate) output: Vec<u8>,
    /// How long it ran, in seconds, as it says.
    pub(crate) seconds: String,
    /// Why it counts as failed, when it does.
    pub(crate) failure: Option<String>,
}

impl Launched {
    /// Wait for every binary, print each one's output in the order cargo
    /// started them, and say whether all passed.
    ///
    /// # Errors
    ///
    /// A binary that failed or never reported, a record and cargo's lines
    /// that disagree, or no binary at all.
    pub(crate) fn finish(self) -> Result<()> {
        let ended: Vec<Ended> = commands(&self.directory)
            .into_iter()
            .map(|index| wait_for(&self.directory, index))
            .collect();
        let mut failures: Vec<String> = Vec::new();
        for one in &ended {
            println!(
                "\n     Ran {} (side by side): {}, {} s",
                one.command.join(" "),
                one.status,
                one.seconds
            );
            print!("{}", String::from_utf8_lossy(&one.output));
            if let Some(why) = &one.failure {
                failures.push(format!("{}: {why}", one.command.join(" ")));
            }
        }
        failures.extend(disagreements(&self.running, &ended));
        if ended.is_empty() {
            failures.push("cargo test started no test binary through the runner".to_owned());
        }
        let failed = ended.iter().filter(|one| one.failure.is_some()).count();
        println!(
            "\n-- the test binaries, {AT_ONCE} at once: {} ran, {} passed, {failed} failed; {:.1} s after cargo returned",
            ended.len(),
            ended.len() - failed,
            self.began.elapsed().as_secs_f64()
        );
        let _ = std::fs::remove_dir_all(&self.directory);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::new(format!(
                "test binaries failed:\n  {}",
                failures.join("\n  ")
            )))
        }
    }
}

/// Where cargo's "Running" lines and the records disagree: a binary named
/// in one and not the other, or not as often.
pub(crate) fn disagreements(running: &[String], ended: &[Ended]) -> Vec<String> {
    let mut named: Vec<&str> = running.iter().map(String::as_str).collect();
    let mut recorded: Vec<&str> = ended
        .iter()
        .filter_map(|one| one.command.first().map(String::as_str))
        .collect();
    named.sort_unstable();
    recorded.sort_unstable();
    if named == recorded {
        return Vec::new();
    }
    let all: BTreeSet<&str> = named.iter().chain(recorded.iter()).copied().collect();
    all.into_iter()
        .filter_map(|binary| {
            let said = named.iter().filter(|name| **name == binary).count();
            let ran = recorded.iter().filter(|name| **name == binary).count();
            (said != ran).then(|| {
                format!("{binary}: cargo said it ran {said} time(s), the runner recorded {ran}")
            })
        })
        .collect()
}

/// The records of commands in `directory`: `<n>.cmd`, by `n`.
fn commands(directory: &Path) -> Vec<usize> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found: Vec<usize> = entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".cmd")?
                .parse()
                .ok()
        })
        .collect();
    found.sort_unstable();
    found
}

/// Wait for binary `index` to end, and read how.
pub(crate) fn wait_for(directory: &Path, index: usize) -> Ended {
    let record = |suffix: &str| directory.join(format!("{index}.{suffix}"));
    let command: Vec<String> = std::fs::read_to_string(record("cmd"))
        .map(|text| text.split('\0').map(str::to_owned).collect())
        .unwrap_or_else(|_| vec![format!("test binary {index}")]);
    // As long as it runs; a waiter gone without a status is a failure.
    while !record("status").exists() && !waiter_gone(directory, index) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = std::fs::read(record("out")).unwrap_or_default();
    let seconds = std::fs::read_to_string(record("seconds"))
        .map_or_else(|_| "?".to_owned(), |text| text.trim().to_owned());
    let Ok(status) = std::fs::read_to_string(record("status")) else {
        return Ended {
            command,
            status: "no status".to_owned(),
            output,
            seconds,
            failure: Some("it never reported how it ended".to_owned()),
        };
    };
    let status = status.trim().to_owned();
    let failure = if status == "0" {
        // The files it ran, unchanged since it started (a later build could
        // have rewritten them under it).
        match std::fs::read_to_string(record("digests")) {
            Ok(recorded) => changed(&recorded),
            Err(error) => Some(format!("no record of its files: {error}")),
        }
    } else {
        Some(format!("it ended with {status}"))
    };
    Ended {
        command,
        status,
        output,
        seconds,
        failure,
    }
}

/// Why the files a `.digests` record lists are not what it says now --
/// each path it names, hashed again (the paths `test-wait` saw, the
/// binary's `CARGO_BIN_EXE_*` among them) -- or `None` when every one is.
pub(crate) fn changed(recorded: &str) -> Option<String> {
    if recorded.trim().is_empty() {
        return Some("its record of its files is empty".to_owned());
    }
    for line in recorded.lines() {
        let Some((digest, path)) = line.split_once(' ') else {
            return Some(format!("a record line that does not parse: {line}"));
        };
        match std::fs::read(path) {
            Ok(bytes) if sha256::hex(&bytes) == digest => {}
            Ok(_) => return Some(format!("{path} changed while it ran")),
            Err(error) => return Some(format!("{path} cannot be read again: {error}")),
        }
    }
    None
}

/// Whether binary `index`'s waiter is gone: its `.waiting` record removed
/// (it wrote a status first), or naming a process that no longer exists.
fn waiter_gone(directory: &Path, index: usize) -> bool {
    match std::fs::read_to_string(directory.join(format!("{index}.waiting"))) {
        Err(_) => true,
        // Written by the runner once the waiter started.
        Ok(pid) if !pid.trim().is_empty() => !Path::new("/proc").join(pid.trim()).exists(),
        Ok(_) => false,
    }
}

/// The `CARGO_BIN_EXE_*` paths in this process's environment, sorted.
fn environment_binaries() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::env::vars_os()
        .filter(|(key, _)| key.to_string_lossy().starts_with("CARGO_BIN_EXE_"))
        .map(|(_, value)| PathBuf::from(value))
        .collect();
    found.sort();
    found
}

/// The SHA-256 of a command's binary and of `others`, one per line.
fn digests(command: &[String], others: &[PathBuf]) -> std::io::Result<String> {
    let mut text = String::new();
    let binary = command.first().map(PathBuf::from).unwrap_or_default();
    for path in std::iter::once(&binary).chain(others) {
        let bytes = std::fs::read(path)?;
        text.push_str(&format!("{} {}\n", sha256::hex(&bytes), path.display()));
    }
    Ok(text)
}

/// `test-runner` and `test-wait`, run when they are this program's command:
/// they take a test binary's own arguments, which xtask's parser does not
/// know. `None` for any other command.
pub(crate) fn command() -> Option<Result<()>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let rest = raw.get(1..).unwrap_or_default();
    match raw.first().map(String::as_str) {
        Some("test-runner") => Some(runner(rest)),
        Some("test-wait") => Some(wait(rest)),
        _ => None,
    }
}

/// The directory `FERRIX_TEST_RUNS` names.
fn runs_directory(who: &str) -> Result<PathBuf> {
    std::env::var_os(VARIABLE)
        .map(PathBuf::from)
        .ok_or_else(|| Error::new(format!("{who}: FERRIX_TEST_RUNS names no directory")))
}

/// `xtask test-runner <binary> <arguments>`, as cargo calls it: record the
/// command, start `test-wait` for it, and return.
///
/// # Errors
///
/// No directory to record in, or the waiter not starting: cargo then fails.
pub(crate) fn runner(arguments: &[String]) -> Result<()> {
    let directory = runs_directory("test-runner")?;
    if arguments.is_empty() {
        return Err(Error::new("test-runner: no test binary"));
    }
    // The next free number, taken by creating its file.
    let mut index = 0;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(format!("{index}.cmd")))
        {
            Ok(mut file) => {
                use std::io::Write as _;
                file.write_all(arguments.join("\0").as_bytes())?;
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => index += 1,
            Err(error) => return Err(error.into()),
        }
    }
    start_waiter(&directory, index, arguments, &std::env::current_exe()?)
}

/// Start `exe test-wait` for binary `index`. Once `<index>.cmd` exists, any
/// failure leaves a status that says so, so the collector fails rather than
/// waits.
fn start_waiter(directory: &Path, index: usize, arguments: &[String], exe: &Path) -> Result<()> {
    let started = (|| -> Result<()> {
        // Marked before the waiter starts, so `finish` never sees a gap.
        std::fs::write(directory.join(format!("{index}.waiting")), b"")?;
        let output = std::fs::File::create(directory.join(format!("{index}.out")))?;
        let child = Command::new(exe)
            .arg("test-wait")
            .arg(index.to_string())
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output)
            .spawn()
            .map_err(|error| {
                Error::new(format!("test-runner: could not start test-wait: {error}"))
            })?;
        std::fs::write(
            directory.join(format!("{index}.waiting")),
            child.id().to_string(),
        )?;
        Ok(())
    })();
    if let Err(error) = &started {
        let _ = std::fs::write(
            directory.join(format!("{index}.status")),
            format!("could not start test-wait: {error}"),
        );
        let _ = std::fs::remove_file(directory.join(format!("{index}.waiting")));
    }
    started
}

/// `xtask test-wait <index> <binary> <arguments>`: take a slot, run the
/// binary with cargo's environment, and write how it ended.
///
/// # Errors
///
/// No directory, or its records not written (`finish` then fails too).
pub(crate) fn wait(arguments: &[String]) -> Result<()> {
    let directory = runs_directory("test-wait")?;
    let (index, command) = arguments
        .split_first()
        .ok_or_else(|| Error::new("test-wait: no index"))?;
    let owner = std::env::var(OWNER).unwrap_or_default();
    let record = |suffix: &str| directory.join(format!("{index}.{suffix}"));
    let status = match take_slot(&directory) {
        Err(error) => format!("no slot: {error}"),
        Ok(_slot) => {
            std::fs::write(
                record("digests"),
                digests(command, &environment_binaries())?,
            )?;
            let began = Instant::now();
            let status = run(command, &owner);
            std::fs::write(
                record("seconds"),
                format!("{:.1}", began.elapsed().as_secs_f64()),
            )?;
            status
        }
    };
    let temporary = record("status.part");
    std::fs::write(&temporary, status)?;
    std::fs::rename(&temporary, record("status"))?;
    let _ = std::fs::remove_file(record("waiting"));
    Ok(())
}

/// Run `command` with this process's environment less this module's
/// variables, while the `check` named `owner` lives: its status, `0` for
/// success.
pub(crate) fn run(command: &[String], owner: &str) -> String {
    let Some((binary, rest)) = command.split_first() else {
        return "no test binary".to_owned();
    };
    let child = Command::new(binary)
        .args(rest)
        .env_remove(VARIABLE)
        .env_remove(OWNER)
        .stdin(Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(error) => return format!("could not start: {error}"),
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return status
                    .code()
                    .map_or_else(|| format!("a signal ({status})"), |code| code.to_string());
            }
            Ok(None) => {}
            Err(error) => return format!("could not be waited for: {error}"),
        }
        if !owner.is_empty() && !Path::new("/proc").join(owner).exists() {
            let _ = child.kill();
            let _ = child.wait();
            return "killed: the check that started it is gone".to_owned();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// One of [`AT_ONCE`] slots, held while the returned file is open.
fn take_slot(directory: &Path) -> Result<std::fs::File> {
    let began = Instant::now();
    loop {
        for slot in 0..AT_ONCE {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(directory.join(format!("slot-{slot}")))?;
            if file.try_lock().is_ok() {
                return Ok(file);
            }
        }
        if began.elapsed() > SLOT_TIMEOUT {
            return Err(Error::new(format!(
                "no slot of {AT_ONCE} freed in {} minutes",
                SLOT_TIMEOUT.as_secs() / 60
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests;
