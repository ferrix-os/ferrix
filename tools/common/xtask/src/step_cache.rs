//! `check`'s units, skipped when every input of a run that passed is
//! unchanged (`docs/TEST-TIME.md`, *Cut B2*).
//!
//! A unit is one workspace of its own -- the compositor's, init's, an
//! app's -- with its formatting, clippy and tests. On a miss it runs as a
//! child, `cargo xtask check-unit`, under `strace -ff`, and when it passes
//! the trace says what it read: every file opened (kept by the SHA-256 of
//! its bytes), every directory listed (by its sorted entries), every path
//! looked up by `stat`, `access` or `readlink` (by what it was: absent, a
//! directory, a file of some size, a link to some target), and every
//! program started, with its interpreter. Those, and a prelude -- xtask's
//! own binary, the environment, the toolchains' version strings and
//! components, the kernel's release, the processor -- are the unit's
//! record. On the next `check` every recorded input is read again, and when
//! each one is what it was, the unit is a *cached pass*: it said "passed"
//! for these exact inputs, and its log line names the run that said so.
//!
//! It fails closed. A trace that does not parse, a unit that opens a network
//! socket, writes outside the target directory and `/tmp`, reads a `/tmp`
//! file it did not make, or reads any file of the certified item (whose
//! steps are never cached, the certification consultant's ruling, ledger
//! line 1447) is run and not recorded; a record that does not parse, names
//! another xtask, is older than [`EXPIRY`], or names an input that can no
//! longer be read is a miss. Only passes are recorded: a failure always runs
//! again.
//!
//! Off unless `FERRIX_STEP_CACHE` is `on` (a store in the target
//! directory); unset, or anything else, runs every unit as before. Release and baseline runs keep it off
//! (`docs/TEST-TIME.md`). Linux only, since it needs `strace`.
//!
//! What it cannot see, the residual `docs/certification/TOOLS.md` names:
//! files read through a descriptor the unit inherited, the network (a unit
//! that opens a socket is not recorded), and the roots in [`EXCLUDED`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use crate::{Error, Result, paths, sha256};

/// The variable that turns the cache on and says where its store is.
pub(crate) const VARIABLE: &str = "FERRIX_STEP_CACHE";

/// The variable that names the unit to a `check-unit` child.
pub(crate) const UNIT_VARIABLE: &str = "FERRIX_CHECK_UNIT";

/// The first line of a record, with the format's version.
const MAGIC: &str = "ferrix-step-cache 1";

/// How long a record stands: a week, after which the unit runs again
/// whatever its inputs say (the consultant's condition 5).
const EXPIRY: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Records kept per unit, newest first.
const KEPT: usize = 8;

/// Units that may be cached although an open flake row in
/// `docs/BACKLOG.md` names them, each with that row, whose flake is a false
/// failure under load (the consultant's ruling, ledger line 1461). The log
/// prints the row beside the unit's cached pass. Every other unit a flake
/// row names is in [`NEVER`].
pub(crate) const FLAKY_BUT_CACHED: &[(&str, &str)] = &[
    (
        "compositor",
        "BACKLOG P1 flakes: host tests of the compositor failed `cargo xtask check` under load (hyprix two_clients), each passing on its rerun",
    ),
    (
        "app:waybar",
        "BACKLOG P1 flakes: host tests of the compositor failed `cargo xtask check` under load (waybar against_hyprix), each passing on its rerun",
    ),
];

/// Units never cached.
const NEVER: &[&str] = &[];

/// Roots whose files are not inputs, each with why. A path under one is
/// neither recorded nor checked.
const EXCLUDED: &[(&str, &str)] = &[
    (
        "${TARGET}",
        "the unit's own target directory: its outputs, judged by cargo's own fingerprints",
    ),
    ("/proc", "the kernel's view of running processes"),
    ("/sys", "the kernel's view of devices"),
    ("/dev", "device nodes"),
    ("/run", "runtime state of services"),
    (
        "${RUSTUP_HOME}/toolchains",
        "the toolchains, recorded by their version strings and components instead",
    ),
    ("${RUSTUP_HOME}/downloads", "rustup's downloads"),
    ("${RUSTUP_HOME}/tmp", "rustup's scratch"),
    (
        "${CARGO_HOME}/registry/index",
        "the registry's index: resolution is pinned by Cargo.lock, which is recorded",
    ),
    (
        "${CARGO_HOME}/registry/cache",
        "packed crates; the unpacked sources a build reads are recorded",
    ),
    (
        "${CARGO_HOME}/git/db",
        "git dependencies' object stores; the checkouts a build reads are recorded",
    ),
    (
        "${CARGO_HOME}/.package-cache",
        "cargo's lock on its package cache",
    ),
    (
        "${CARGO_HOME}/.package-cache-mutate",
        "cargo's lock on its package cache",
    ),
    (
        "${CARGO_HOME}/.global-cache",
        "cargo's record of when it last used each download",
    ),
    (
        "${CARGO_HOME}/.global-cache-journal",
        "cargo's record of when it last used each download",
    ),
    (
        "${CARGO_HOME}/.global-cache-wal",
        "cargo's record of when it last used each download",
    ),
    (
        "${CARGO_HOME}/.global-cache-shm",
        "cargo's record of when it last used each download",
    ),
    (
        "${HOME}/.cache/nvidia",
        "the NVIDIA driver's shader cache, keyed by the driver on the shader and its own version",
    ),
    (
        "${HOME}/.cache/mesa_shader_cache",
        "Mesa's shader cache, keyed by Mesa on the shader and its own version",
    ),
    (
        "${HOME}/.cache/mesa_shader_cache_db",
        "Mesa's shader cache, keyed by Mesa on the shader and its own version",
    ),
];

/// Environment variables left out of the prelude: a terminal's and a
/// session's, which no step reads, and the cache's own.
const ENVIRONMENT_IGNORED: &[&str] = &[
    "CARGO_TARGET_DIR",
    "CLAUDECODE",
    "COLORTERM",
    "DBUS_SESSION_BUS_ADDRESS",
    "GPG_AGENT_INFO",
    "GPG_TTY",
    "HISTFILE",
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "LS_COLORS",
    "MAIL",
    "MANAGERPID",
    "OLDPWD",
    "PWD",
    "SHLVL",
    "SYSTEMD_EXEC_PID",
    "TERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "VTE_VERSION",
    "_",
    UNIT_VARIABLE,
    VARIABLE,
];

/// Prefixes of environment variables left out, for the same reason.
const ENVIRONMENT_IGNORED_PREFIXES: &[&str] = &[
    "ANTHROPIC_",
    "CLAUDE_",
    "OTEL_",
    "SSH_",
    "TMUX",
    "XDG_ACTIVATION",
    "XDG_CURRENT_DESKTOP",
    "XDG_GREETER",
    "XDG_MENU_PREFIX",
    "XDG_SEAT",
    "XDG_SESSION",
    "XDG_VTNR",
];

/// Where the store is, or `None` when the cache is off: `on` keeps it in
/// the target directory. A store shared between target directories (gate
/// slots) waits for a control that shows a record made in one worktree
/// gives the verdict a fresh run gives in another (the consultant's
/// condition 5), so no other value turns it on.
pub(crate) fn store() -> Option<PathBuf> {
    (std::env::var_os(VARIABLE)? == "on").then(|| paths::target_dir().join("step-cache"))
}

/// How a unit ended in this `check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It ran.
    Ran,
    /// A record of a pass on the same inputs stood for it.
    Cached,
}

/// Run `unit` through the cache: answered by a record when one stands,
/// otherwise run under `strace` as `cargo xtask check-unit` and recorded
/// when it passes; `inline` when the cache is off or cannot trace.
///
/// # Errors
///
/// The unit failing.
pub(crate) fn unit(
    name: &str,
    target: &Path,
    inline: impl FnOnce() -> Result<()>,
) -> Result<Outcome> {
    let Some(store) = store() else {
        inline()?;
        return Ok(Outcome::Ran);
    };
    if NEVER.contains(&name) {
        println!("   {name}: ran (never cached)");
        inline()?;
        return Ok(Outcome::Ran);
    }
    if !cfg!(target_os = "linux") || !strace_works() {
        println!("   {name}: ran (no strace, so nothing is recorded)");
        inline()?;
        return Ok(Outcome::Ran);
    }
    let mut context = Context::once()?.clone();
    context.unit_target = absolute(target)?;
    let context = &context;
    let directory = store.join(file_name(name));
    if let Some((origin, inputs)) = lookup(name, &directory, context) {
        let flake = FLAKY_BUT_CACHED
            .iter()
            .find(|(unit, _)| *unit == name)
            .map_or(String::new(), |(_, row)| format!(" [flake row: {row}]"));
        println!("   {name}: cached pass {origin} ({inputs} inputs unchanged){flake}");
        return Ok(Outcome::Cached);
    }
    run_traced(name, &directory, context)?;
    Ok(Outcome::Ran)
}

/// A unit's name as a directory's.
fn file_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Whether `strace` runs here.
fn strace_works() -> bool {
    Command::new("strace")
        .arg("-V")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Run the unit as a traced child, and record it when it passes and the
/// trace allows.
fn run_traced(name: &str, directory: &Path, context: &Context) -> Result<()> {
    std::fs::create_dir_all(directory)?;
    let traces = directory.join(format!("trace-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&traces);
    std::fs::create_dir_all(&traces)?;
    let status = Command::new("strace")
        .args([
            "-ff",
            "-qq",
            "-y",
            "-ttt",
            "-s",
            "4096",
            "-e",
            "signal=none",
        ])
        .args([
            "-e",
            "trace=%file,%process,%network,chdir,fchdir,io_uring_setup,pidfd_getfd",
        ])
        .arg("-o")
        .arg(traces.join("t"))
        .arg("--")
        .arg(std::env::current_exe()?)
        .arg("check-unit")
        .current_dir(&context.root)
        .env(UNIT_VARIABLE, name)
        .status()
        .map_err(|error| Error::new(format!("could not start strace for {name}: {error}")))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&traces);
        return Err(Error::new(format!(
            "{name} failed ({status}; it ran as `cargo xtask check-unit` under strace)"
        )));
    }
    let made = record(name, &traces, context);
    let _ = std::fs::remove_dir_all(&traces);
    match made {
        Ok(text) => {
            write_record(directory, &text)?;
            println!("   {name}: ran, recorded");
        }
        Err(why) => println!("   {name}: ran, not recorded: {why}"),
    }
    Ok(())
}

/// Where this run is, and what it is run with.
#[derive(Debug, Clone)]
pub(crate) struct Context {
    /// The worktree.
    root: PathBuf,
    /// The target directory.
    target: PathBuf,
    /// `$HOME`.
    home: PathBuf,
    /// `$CARGO_HOME`, or `~/.cargo`.
    cargo_home: PathBuf,
    /// `$RUSTUP_HOME`, or `~/.rustup`.
    rustup_home: PathBuf,
    /// The temporary directory.
    temporary: PathBuf,
    /// The prelude's lines but the toolchains': xtask, environment, host.
    prelude: Vec<String>,
    /// Where the record says the pass came from.
    origin: String,
    /// The certified item's paths, relative to the root, each ending in `/`.
    item: Vec<String>,
    /// xtask's own binary, which every unit starts.
    xtask: PathBuf,
    /// The unit's own target directory, the one part of the target
    /// directory whose files it may read without having made them.
    unit_target: PathBuf,
    /// `$XDG_RUNTIME_DIR`, a temporary directory like `/tmp`.
    runtime: Option<PathBuf>,
}

/// This process's [`Context`], made once: xtask's own digest alone is a
/// read of its whole binary.
static CONTEXT: std::sync::OnceLock<std::result::Result<Context, String>> =
    std::sync::OnceLock::new();

impl Context {
    /// This process's, made the first time it is asked for.
    fn once() -> Result<&'static Context> {
        CONTEXT
            .get_or_init(|| Context::here().map_err(|error| error.to_string()))
            .as_ref()
            .map_err(|error| Error::new(error.clone()))
    }

    /// This process's.
    fn here() -> Result<Context> {
        let root = std::fs::canonicalize(paths::workspace_root())?;
        let target = absolute(&paths::target_dir())?;
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let cargo_home =
            std::env::var_os("CARGO_HOME").map_or_else(|| home.join(".cargo"), PathBuf::from);
        let rustup_home =
            std::env::var_os("RUSTUP_HOME").map_or_else(|| home.join(".rustup"), PathBuf::from);
        let mut context = Context {
            root,
            target,
            home,
            cargo_home,
            rustup_home,
            temporary: std::env::temp_dir(),
            prelude: Vec::new(),
            origin: String::new(),
            item: Vec::new(),
            xtask: std::env::current_exe()?,
            unit_target: PathBuf::new(),
            runtime: std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        };
        let exe = std::fs::read(std::env::current_exe()?)?;
        context.prelude = vec![
            format!("xtask {}", sha256::hex(&exe)),
            format!("env {}", context.environment_digest()),
            format!("host {}", host_digest()),
        ];
        context.origin = origin(&context.root, &context.target);
        context.item = item_paths(&context.root)?;
        Ok(context)
    }

    /// The digest of the environment, paths of the root and the target
    /// directory written as placeholders so that two slots agree.
    fn environment_digest(&self) -> String {
        let mut variables: Vec<(String, String)> = std::env::vars_os()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .filter(|(key, _)| !environment_ignored(key))
            .map(|(key, value)| (key, self.placeholders(&value)))
            .collect();
        variables.sort();
        let text: String = variables
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        sha256::hex(text.as_bytes())
    }

    /// `text` with the target directory and the root written as
    /// placeholders.
    fn placeholders(&self, text: &str) -> String {
        text.replace(&*self.target.to_string_lossy(), "${TARGET}")
            .replace(&*self.root.to_string_lossy(), "${ROOT}")
    }

    /// The excluded roots, as paths here.
    fn excluded(&self) -> Vec<(PathBuf, &'static str)> {
        EXCLUDED
            .iter()
            .map(|(root, why)| {
                let path = root
                    .replace("${TARGET}", &self.unit_target.to_string_lossy())
                    .replace("${RUSTUP_HOME}", &self.rustup_home.to_string_lossy())
                    .replace("${CARGO_HOME}", &self.cargo_home.to_string_lossy())
                    .replace("${HOME}", &self.home.to_string_lossy());
                (PathBuf::from(path), *why)
            })
            .collect()
    }

    /// Whether `path` is under an excluded root.
    fn is_excluded(&self, path: &Path) -> bool {
        self.excluded()
            .iter()
            .any(|(root, _)| path.starts_with(root))
    }

    /// The temporary directories: what a unit finds in them is no input,
    /// and it may read only what it made there.
    fn temporaries(&self) -> Vec<&Path> {
        let mut roots = vec![
            self.temporary.as_path(),
            Path::new("/tmp"),
            Path::new("/var/tmp"),
            Path::new("/dev/shm"),
        ];
        roots.extend(self.runtime.as_deref());
        roots
    }

    /// Whether `path` is a temporary directory or under one.
    fn is_temporary(&self, path: &Path) -> bool {
        self.temporaries().iter().any(|root| path.starts_with(root))
    }

    /// Whether `path` is a temporary directory itself, which any unit may
    /// look at.
    fn is_temporary_root(&self, path: &Path) -> bool {
        self.temporaries().contains(&path)
    }

    /// The toolchain directory `path` is in, when it is in one.
    fn toolchain_of(&self, path: &Path) -> Option<PathBuf> {
        let toolchains = self.rustup_home.join("toolchains");
        let rest = path.strip_prefix(&toolchains).ok()?;
        let first = rest.components().next()?;
        Some(toolchains.join(first))
    }

    /// `path` as a record writes it: `./<relative>` inside the root.
    fn written(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) => format!("./{}", relative.to_string_lossy()),
            Err(_) => path.to_string_lossy().into_owned(),
        }
    }

    /// A path a record wrote, as a path here.
    fn read_back(&self, written: &str) -> PathBuf {
        match written.strip_prefix("./") {
            Some(relative) => self.root.join(relative),
            None => PathBuf::from(written),
        }
    }

    /// The item path `path` is in, when it is in one.
    fn item_path(&self, path: &Path) -> Option<&str> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let relative = format!("{}/", relative.to_string_lossy());
        self.item
            .iter()
            .find(|prefix| relative.starts_with(prefix.as_str()))
            .map(String::as_str)
    }
}

/// Whether an environment variable is left out of the prelude.
fn environment_ignored(key: &str) -> bool {
    ENVIRONMENT_IGNORED.contains(&key)
        || ENVIRONMENT_IGNORED_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// `path`, absolute.
fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    })
}

/// The kernel's release, the processor's model and flags and the number of
/// processors: what a JIT renderer (llvmpipe, lavapipe) and a test's
/// timing see.
fn host_digest() -> String {
    sha256::hex(host_text().as_bytes())
}

/// What [`host_digest`] hashes.
fn host_text() -> String {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let first = |key: &str| {
        cpuinfo
            .lines()
            .find(|line| line.starts_with(key))
            .unwrap_or_default()
            .to_owned()
    };
    let processors = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    // Which render devices there are: a GPU's tests take another path.
    let mut gpus: Vec<String> = std::fs::read_dir("/dev/dri")
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    gpus.sort();
    format!(
        "{}\n{}\n{}\n{processors}\n{}\n",
        release.trim(),
        first("model name"),
        first("flags"),
        gpus.join(" ")
    )
}

/// Where a pass came from: when, the commit and whether the tree was
/// clean, the target directory (which names a gate slot), and the gate's
/// tag when `gate.sh` says it.
fn origin(root: &Path, target: &Path) -> String {
    let git = |arguments: &[&str]| {
        Command::new("git")
            .current_dir(root)
            .args(arguments)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let commit = git(&["rev-parse", "--short=12", "HEAD"]);
    let dirty = if git(&["status", "--porcelain", "--untracked-files=no"]).is_empty() {
        ""
    } else {
        "+changes"
    };
    let tag = std::env::var("FERRIX_GATE_TAG").map_or(String::new(), |tag| format!(" tag {tag}"));
    format!(
        "from {} on {commit}{dirty}, {}{tag}",
        utc_now(),
        target.display()
    )
}

/// Now, as `date -u +%FT%TZ` writes it, from the seconds since 1970.
fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days);
    let rest = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The proleptic Gregorian date `days` after 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

/// The certified item's paths, relative to the root: the kernel, the
/// loader, the crates `certification-item.json` puts in the item or the
/// core, and the crates on its infrastructure allow-list, which item
/// crates build on.
fn item_paths(root: &Path) -> Result<Vec<String>> {
    let mut item = vec!["src/kernel/".to_owned(), "src/boot/".to_owned()];
    let classification = root.join("tools/common/data/certification-item.json");
    let text = std::fs::read_to_string(&classification)
        .map_err(|error| Error::new(format!("{}: {error}", classification.display())))?;
    // The file's crate entries carry `"ring": "<ring>", "path": "<path>"`
    // in that order; the allow-list names packages, found in the
    // workspace manifest.
    let mut ring = "";
    let mut kept = 0;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = json_string(line, "ring") {
            ring = if value == "item" || value == "core" {
                kept += 1;
                "kept"
            } else {
                ""
            };
        } else if let Some(value) = json_string(line, "path")
            && ring == "kept"
        {
            item.push(format!("{}/", value.trim_end_matches('/')));
            ring = "";
        }
    }
    // Fail closed: every item or core crate's path found, or no cache
    // (the consultant's C5, ledger 1494).
    if item.len() != 2 + kept {
        return Err(Error::new(format!(
            "certification-item.json names {kept} item or core crates, and {} paths were read for them",
            item.len() - 2
        )));
    }
    let manifest = std::fs::read_to_string(root.join("Cargo.toml"))?;
    let allowed = allow_listed(&text);
    if text.contains("\"infrastructure_allowlist\"") && allowed.is_empty() {
        return Err(Error::new(
            "certification-item.json has an infrastructure allow-list, and no crate was read from it",
        ));
    }
    for package in allowed {
        let prefix = format!("{package} = {{ path = \"");
        if let Some(rest) = manifest
            .lines()
            .find_map(|line| line.trim().strip_prefix(prefix.as_str()))
            && let Some(path) = rest.split('"').next()
        {
            item.push(format!("{path}/"));
        } else {
            return Err(Error::new(format!(
                "the item's allow-listed crate {package} is not in the workspace manifest"
            )));
        }
    }
    Ok(item)
}

/// `"key": "value"` at the start of `line`'s JSON, when it is that key.
fn json_string<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(&format!("\"{key}\": \""))?;
    rest.split('"').next()
}

/// The package names under `infrastructure_allowlist` in the item's
/// classification.
fn allow_listed(text: &str) -> Vec<String> {
    let Some(start) = text.find("\"infrastructure_allowlist\"") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    // The key's own line opens the object.
    let mut depth = 1_i32;
    for line in text.get(start..).unwrap_or_default().lines().skip(1) {
        let trimmed = line.trim();
        if depth == 1
            && let Some(rest) = trimmed.strip_prefix('"')
            && let Some(name) = rest.split('"').next()
            && !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            && rest
                .get(name.len()..)
                .is_some_and(|after| after.starts_with("\": "))
        {
            names.push(name.to_owned());
        }
        depth += i32::try_from(line.matches('{').count()).unwrap_or(0);
        depth -= i32::try_from(line.matches('}').count()).unwrap_or(0);
        if depth <= 0 {
            break;
        }
    }
    names
}

// -- the trace -------------------------------------------------------------

/// One input of a unit, as a record keeps it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Input {
    /// A file read, by the digest of its bytes.
    File(PathBuf),
    /// A directory listed, by its sorted entries.
    Directory(PathBuf),
    /// A path looked up, following links or not: what it was.
    Probe(PathBuf, bool),
    /// A link read.
    Link(PathBuf),
}

/// What a trace says a unit did.
#[derive(Debug, Default)]
pub(crate) struct Traced {
    /// Everything it read or looked up.
    pub(crate) inputs: BTreeSet<Input>,
    /// The first reason not to record it, when there is one.
    pub(crate) refused: Option<String>,
}

impl Traced {
    fn refuse(&mut self, why: String) {
        let _ = self.refused.get_or_insert(why);
    }
}

/// One system call, parsed from a line of `strace -y`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    /// Its name.
    pub(crate) name: String,
    /// Its arguments' text, as strace wrote them.
    pub(crate) arguments: String,
    /// What it returned: the number, or `-1`.
    pub(crate) returned: i64,
    /// The error's name, when it failed.
    pub(crate) error: Option<String>,
    /// The path a returned descriptor names (`= 3</path>`).
    pub(crate) returned_path: Option<String>,
    /// When it was made, in microseconds, as `strace -ttt` stamps it; 0
    /// for a line without a stamp.
    pub(crate) time: u64,
}

/// A line's `strace -ttt` stamp, `<seconds>.<microseconds> `, taken off:
/// the time in microseconds and the rest.
fn stamp(line: &str) -> (u64, &str) {
    if let Some((first, rest)) = line.split_once(' ')
        && let Some((seconds, micros)) = first.split_once('.')
        && let (Ok(seconds), Ok(micros)) = (seconds.parse::<u64>(), micros.parse::<u64>())
        && micros < 1_000_000
    {
        return (seconds * 1_000_000 + micros, rest);
    }
    (0, line)
}

/// Parse one line, `None` for a line that is no call (a signal, an exit).
///
/// # Errors
///
/// A line that is a call but does not parse.
pub(crate) fn parse_line(line: &str) -> std::result::Result<Option<Call>, String> {
    let (time, line) = stamp(line.trim_end());
    // `<detached ...>`: a thread that left in the middle of a call strace
    // could not name, as a process exits.
    if line.starts_with("+++")
        || line.starts_with("---")
        || line.is_empty()
        || line.ends_with("<detached ...>")
    {
        return Ok(None);
    }
    let Some((name, _)) = line.split_once('(') else {
        return Err(format!("no call in {line:?}"));
    };
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("no call name in {line:?}"));
    }
    // strace pads the arguments to a column before ` = `.
    let Some((call, result)) = line.rsplit_once(" = ") else {
        return Err(format!("no result in {line:?}"));
    };
    let Some(arguments) = call
        .trim_end()
        .strip_suffix(')')
        .and_then(|call| call.split_once('('))
        .map(|(_, arguments)| arguments.to_owned())
    else {
        return Err(format!("no closing parenthesis in {line:?}"));
    };
    if result.starts_with('?') {
        return Ok(None);
    }
    let number: String = result
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-' || *c == 'x' || c.is_ascii_hexdigit())
        .collect();
    let returned = if let Some(hex) = number.strip_prefix("0x") {
        i64::from_str_radix(hex, 16).map_err(|_| format!("bad result in {line:?}"))?
    } else {
        number
            .parse::<i64>()
            .map_err(|_| format!("bad result in {line:?}"))?
    };
    let rest = result.get(number.len()..).unwrap_or_default();
    let error = if returned < 0 {
        rest.split_whitespace().next().map(str::to_owned)
    } else {
        None
    };
    let returned_path = rest
        .strip_prefix('<')
        .and_then(|inside| inside.rsplit_once('>'))
        .map(|(path, _)| path.to_owned());
    Ok(Some(Call {
        name: name.to_owned(),
        arguments,
        returned,
        error,
        returned_path,
        time,
    }))
}

/// The quoted strings in `arguments`, unescaped, each with the directory
/// the descriptor before it names (`AT_FDCWD</dir>`, `3</dir>`), if any.
pub(crate) fn strings(
    arguments: &str,
) -> std::result::Result<Vec<(String, Option<String>)>, String> {
    let bytes = arguments.as_bytes();
    let mut found = Vec::new();
    let mut directory: Option<String> = None;
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'<' => {
                let rest = arguments
                    .get(index + 1..)
                    .ok_or_else(|| format!("a <...> inside a character in {arguments:?}"))?;
                let (inside, _) = rest
                    .split_once(">,")
                    .or_else(|| rest.rsplit_once('>'))
                    .ok_or_else(|| format!("unclosed <...> in {arguments:?}"))?;
                directory = Some(inside.to_owned());
                index += inside.len() + 2;
            }
            b'"' => {
                let (text, next) = unquote(bytes, index + 1)?;
                found.push((text, directory.take()));
                index = next;
            }
            b'[' | b'{' => {
                // argv, envp and structures: skip to the matching close,
                // strings inside included.
                index = skip_group(bytes, index)?;
            }
            _ => index += 1,
        }
    }
    Ok(found)
}

/// The string starting after the quote at `start - 1`, and the index past
/// its closing quote.
fn unquote(bytes: &[u8], start: usize) -> std::result::Result<(String, usize), String> {
    let mut out = Vec::new();
    let mut index = start;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => {
                // A string strace cut short ("..."...) is marked like one
                // that is not UTF-8: refused if it is used as a path.
                if bytes.get(index + 1..index + 4) == Some(b"...") {
                    let text = format!("\0{}", String::from_utf8_lossy(&out));
                    return Ok((text, index + 4));
                }
                // Not every string is a path (an extended attribute's
                // value, say); one that is not UTF-8 is marked with a NUL,
                // which no path holds, and refused if used as a path.
                let text = String::from_utf8(out).unwrap_or_else(|error| {
                    format!("\0{}", String::from_utf8_lossy(error.as_bytes()))
                });
                return Ok((text, index + 1));
            }
            b'\\' => {
                let escape = *bytes.get(index + 1).ok_or("a dangling escape")?;
                match escape {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'r' => out.push(b'\r'),
                    b'v' => out.push(0x0b),
                    b'f' => out.push(0x0c),
                    b'"' | b'\\' => out.push(escape),
                    b'x' => {
                        let hex = std::str::from_utf8(
                            bytes.get(index + 2..index + 4).ok_or("short \\x")?,
                        )
                        .map_err(|_| "bad \\x")?;
                        out.push(u8::from_str_radix(hex, 16).map_err(|_| "bad \\x")?);
                        index += 2;
                    }
                    b'0'..=b'7' => {
                        let mut value = 0_u32;
                        let mut digits = 0;
                        while digits < 3
                            && let Some(&digit @ b'0'..=b'7') = bytes.get(index + 1 + digits)
                        {
                            value = value * 8 + u32::from(digit - b'0');
                            digits += 1;
                        }
                        out.push(u8::try_from(value).map_err(|_| "bad octal escape")?);
                        index += digits - 1;
                    }
                    _ => return Err(format!("unknown escape \\{}", char::from(escape))),
                }
                index += 2;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    Err("an unclosed string".to_owned())
}

/// The index past the group opened at `start`.
fn skip_group(bytes: &[u8], start: usize) -> std::result::Result<usize, String> {
    let mut depth = 0_usize;
    let mut index = start;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            b'"' => {
                let (_, next) = unquote(bytes, index + 1)?;
                index = next;
                continue;
            }
            _ => {}
        }
        index += 1;
    }
    Err("an unclosed group".to_owned())
}

/// `path` joined to `base` when relative, with `.` and empty components
/// dropped; `..` is kept for the kernel to resolve as the unit's call did.
fn joined(base: &Path, path: &str) -> PathBuf {
    let full = if path.starts_with('/') {
        PathBuf::from(path)
    } else {
        base.join(path)
    };
    let mut out = PathBuf::from("/");
    for component in full.components() {
        match component {
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Calls that only look a path up.
const PROBES: &[&str] = &[
    "stat",
    "lstat",
    "newfstatat",
    "fstatat64",
    "statx",
    "access",
    "faccessat",
    "faccessat2",
    "statfs",
    "getxattr",
    "lgetxattr",
    "listxattr",
    "llistxattr",
    "chdir",
];

/// Calls that change the file system.
const WRITES: &[&str] = &[
    "mkdir",
    "mkdirat",
    "rmdir",
    "unlink",
    "unlinkat",
    "rename",
    "renameat",
    "renameat2",
    "link",
    "linkat",
    "symlink",
    "symlinkat",
    "chmod",
    "fchmodat",
    "fchmodat2",
    "chown",
    "lchown",
    "fchownat",
    "utime",
    "utimes",
    "utimensat",
    "futimesat",
    "truncate",
    "mknod",
    "mknodat",
    "setxattr",
    "lsetxattr",
    "removexattr",
    "lremovexattr",
    "creat",
];

/// Calls of `%file`, `%process` and `%network` that name no path this
/// cares about.
const IGNORED: &[&str] = &[
    "clone",
    "clone3",
    "fork",
    "vfork",
    "wait4",
    "waitid",
    "kill",
    "tkill",
    "tgkill",
    "exit",
    "exit_group",
    "getcwd",
    "fchdir",
    "socketpair",
    "connect",
    "bind",
    "listen",
    "accept",
    "accept4",
    "getsockname",
    "getpeername",
    "sendto",
    "recvfrom",
    "sendmsg",
    "recvmsg",
    "sendmmsg",
    "recvmmsg",
    "setsockopt",
    "getsockopt",
    "shutdown",
    "pidfd_open",
    "pidfd_send_signal",
    "inotify_add_watch",
    "fanotify_mark",
];

/// Read the trace files `t.<pid>` in `directory`.
///
/// # Errors
///
/// A file that cannot be read, a line that does not parse, or a tree of
/// processes without one root.
pub(crate) fn read_traces(
    directory: &Path,
    root: &Path,
    context: &Context,
) -> std::result::Result<Traced, String> {
    let mut files: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Some(pid) = name
            .strip_prefix("t.")
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            return Err(format!("an unexpected file {name} among the traces"));
        };
        let text = std::fs::read_to_string(&path).map_err(|error| error.to_string())?;
        let _ = files.insert(pid, merge_unfinished(&text)?);
    }
    analyse(&files, root, context)
}

/// A trace's lines with `<unfinished ...>` and `<... resumed>` joined.
fn merge_unfinished(text: &str) -> std::result::Result<Vec<String>, String> {
    let mut lines = Vec::new();
    let mut pending: Option<String> = None;
    for line in text.lines() {
        if let Some(head) = line.strip_suffix(" <unfinished ...>") {
            if pending.is_some() {
                return Err("two calls unfinished at once in one thread".to_owned());
            }
            pending = Some(head.to_owned());
        } else if stamp(line).1.starts_with("<... ") {
            let head = pending.take().ok_or("a resumed call that never started")?;
            let tail = line
                .split_once("resumed>")
                .ok_or("a resumed line without its marker")?
                .1;
            lines.push(format!("{head}{tail}"));
        } else {
            lines.push(line.to_owned());
        }
    }
    if let Some(head) = pending {
        // A call the process never returned from (killed in it, or an
        // execve that replaced it): only a few are harmless.
        if !(head.starts_with("exit") || head.starts_with("execve(")) {
            return Err(format!("a call never finished: {head}"));
        }
    }
    Ok(lines)
}

/// What the parsed traces of each process say, from the root process
/// down, each child starting in its parent's working directory.
fn analyse(
    files: &BTreeMap<u32, Vec<String>>,
    root: &Path,
    context: &Context,
) -> std::result::Result<Traced, String> {
    // Children, from the clone and fork lines.
    let mut parsed: BTreeMap<u32, Vec<Call>> = BTreeMap::new();
    let mut children: BTreeSet<u32> = BTreeSet::new();
    let mut unstamped = None;
    for (pid, lines) in files {
        let mut calls = Vec::new();
        for line in lines {
            if let Some(call) = parse_line(line)? {
                if call.time == 0 {
                    let _ = unstamped.get_or_insert_with(|| line.clone());
                }
                if matches!(call.name.as_str(), "clone" | "clone3" | "fork" | "vfork")
                    && call.returned > 0
                {
                    let _ = children.insert(u32::try_from(call.returned).map_err(|_| "a bad pid")?);
                }
                calls.push(call);
            }
        }
        let _ = parsed.insert(*pid, calls);
    }
    let roots: Vec<u32> = parsed
        .keys()
        .copied()
        .filter(|pid| !children.contains(pid))
        .collect();
    let [first] = roots.as_slice() else {
        return Err(format!(
            "the traces have {} root processes, not one",
            roots.len()
        ));
    };
    // Twice: the first pass finds every path the run made fresh and every
    // socket it bound, in whichever process and when; the second judges each
    // read against them, by the time strace stamped on each call.
    let mut walk = Walk {
        context,
        traced: Traced::default(),
        created: BTreeMap::new(),
        bound: BTreeSet::new(),
        judging: false,
        now: 0,
    };
    for judging in [false, true] {
        walk.traced = Traced::default();
        walk.judging = judging;
        let mut queue = vec![(*first, root.to_path_buf())];
        let mut seen = BTreeSet::new();
        while let Some((pid, mut cwd)) = queue.pop() {
            if !seen.insert(pid) {
                continue;
            }
            let Some(calls) = parsed.get(&pid) else {
                // A thread or child that made no call this traces.
                continue;
            };
            for call in calls {
                walk.now = call.time;
                walk.one_call(call, &mut cwd, &mut queue)?;
            }
        }
    }
    // Without strace's stamps every creation would seem to come before
    // every read, and the order of C2 would go unchecked (the consultant,
    // ledger 1508).
    if let Some(line) = unstamped {
        walk.traced
            .refuse(format!("a call without strace's time stamp: {line}"));
    }
    Ok(walk.traced)
}

/// The analysis of one run's traces.
struct Walk<'a> {
    /// Where the run was.
    context: &'a Context,
    /// What it found.
    traced: Traced,
    /// Paths the run made fresh, by when (microseconds), in the first pass.
    created: BTreeMap<PathBuf, u64>,
    /// Unix socket addresses the run bound.
    bound: BTreeSet<String>,
    /// The second pass, which judges; the first only collects.
    judging: bool,
    /// The time of the call being looked at.
    now: u64,
}

impl Walk<'_> {
    /// Fold one call in.
    fn one_call(
        &mut self,
        call: &Call,
        cwd: &mut PathBuf,
        queue: &mut Vec<(u32, PathBuf)>,
    ) -> std::result::Result<(), String> {
        let name = call.name.as_str();
        let strings = strings(&call.arguments)?;
        // The working directory, as strace names it on any `AT_FDCWD`.
        if let Some((_, rest)) = call.arguments.split_once("AT_FDCWD<")
            && let Some((directory, _)) = rest.split_once(">,").or_else(|| rest.split_once('>'))
        {
            *cwd = PathBuf::from(directory);
        }
        let paths: Vec<PathBuf> = strings
            .iter()
            .map(|(text, directory)| {
                let base = directory
                    .as_ref()
                    .map_or_else(|| cwd.clone(), PathBuf::from);
                joined(&base, text)
            })
            .collect();
        let first = paths.first().cloned();
        match name {
            "clone" | "clone3" | "fork" | "vfork" => {
                if call.returned > 0 {
                    queue.push((
                        u32::try_from(call.returned).map_err(|_| "a bad pid")?,
                        cwd.clone(),
                    ));
                }
            }
            "chdir" => {
                if let Some(path) = first {
                    self.probe(&path, call, true);
                    if call.returned == 0 {
                        *cwd = path;
                    }
                }
            }
            "fchdir" => {
                if call.returned == 0
                    && let Some((_, rest)) = call.arguments.split_once('<')
                    && let Some((directory, _)) = rest.rsplit_once('>')
                {
                    *cwd = PathBuf::from(directory);
                }
            }
            "socket" | "bind" | "connect" | "sendto" | "sendmsg" | "sendmmsg"
            | "io_uring_setup" | "pidfd_getfd" => self.network(call),
            "open" | "openat" | "openat2" | "creat" => {
                let Some(path) = first else {
                    return Err(format!("an {name} without a path"));
                };
                self.open(call, &path);
            }
            "execve" | "execveat" => {
                let Some(path) = first else {
                    return Err(format!("an {name} without a path"));
                };
                if call.returned < 0 {
                    self.absent(&path, call);
                } else {
                    for program in std::iter::once(path.clone()).chain(interpreters(&path)) {
                        self.read(&program, Input::File(program.clone()));
                    }
                }
            }
            "readlink" | "readlinkat" => {
                let Some(path) = first else {
                    return Err(format!("a {name} without a path"));
                };
                if call.returned < 0 {
                    self.absent(&path, call);
                } else {
                    self.read(&path, Input::Link(path.clone()));
                }
            }
            _ if PROBES.contains(&name) => {
                // An empty path with AT_EMPTY_PATH is a descriptor's, already
                // recorded when it was opened.
                if let Some(path) = first
                    && !strings.first().is_some_and(|(text, _)| text.is_empty())
                {
                    let follow = !call.arguments.contains("AT_SYMLINK_NOFOLLOW") && name != "lstat";
                    self.probe(&path, call, follow);
                }
            }
            _ if WRITES.contains(&name) => self.change(call, &paths),
            _ if IGNORED.contains(&name) => {}
            // A number Linux does not know (Ferrix's own calls, which a host
            // test may try): it failed, and did nothing.
            _ if name.starts_with("syscall_0x") && call.returned < 0 => {}
            _ => self.refuse(format!("a call this does not know: {name}")),
        }
        Ok(())
    }

    /// The calls of sockets and of descriptors taken from elsewhere: a
    /// socket of the network, or a Unix one the run did not bind, is a
    /// process this trace does not see.
    fn network(&mut self, call: &Call) {
        let name = call.name.as_str();
        match name {
            "socket" => {
                let domain = call.arguments.split(',').next().unwrap_or_default().trim();
                if call.returned >= 0 && domain != "AF_UNIX" && domain != "AF_LOCAL" {
                    self.refuse(format!("it opened a {domain} socket"));
                }
            }
            "bind" => {
                if call.returned == 0
                    && let Some(address) = unix_address(&call.arguments)
                {
                    let _ = self.bound.insert(address);
                }
            }
            "connect" | "sendto" | "sendmsg" | "sendmmsg" => {
                // Talking to a socket the run did not bind is reading from
                // a process this trace does not see.
                // A socket bound under a name of its own and renamed into
                // place (the compositor's) is the run's too.
                if call.returned >= 0
                    && let Some(address) = unix_address(&call.arguments)
                    && !self.bound.contains(&address)
                    && !(address.starts_with('/') && self.made_before_now(Path::new(&address)))
                {
                    self.refuse(format!(
                        "it talked to the socket {address}, which it did not bind"
                    ));
                }
            }
            "io_uring_setup" | "pidfd_getfd" if call.returned >= 0 => {
                self.refuse(format!("it called {name}, whose reads this cannot trace"));
            }
            _ => {}
        }
    }

    /// Refuse to record, in the judging pass.
    fn refuse(&mut self, why: String) {
        if self.judging {
            self.traced.refuse(why);
        }
    }

    /// Remember that `path` was made fresh now, in the first pass.
    fn made(&mut self, path: &Path) {
        if !self.judging {
            let at = self.created.entry(path.to_path_buf()).or_insert(self.now);
            *at = (*at).min(self.now);
        }
    }

    /// Whether `path`, or a directory above it, was made fresh no later
    /// than now.
    fn made_before_now(&self, path: &Path) -> bool {
        path.ancestors()
            .any(|above| self.created.get(above).is_some_and(|at| *at <= self.now))
    }

    /// An `open`: a file or directory read, a file written, or a lookup
    /// that failed.
    fn open(&mut self, call: &Call, path: &Path) {
        let flag =
            |flag: &str| call.arguments.contains(flag) || call.name == "creat" && flag == "O_TRUNC";
        let writes = ["O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC", "O_TMPFILE"]
            .iter()
            .any(|name| flag(name));
        if call.returned < 0 {
            self.absent(path, call);
            return;
        }
        // A descriptor reopened through `/proc/self/fd` (cargo's jobserver)
        // is a pipe or a socket, no file of any tree.
        if call
            .returned_path
            .as_ref()
            .is_some_and(|opened| !opened.starts_with('/'))
        {
            if !path.starts_with("/proc") && !path.starts_with("/dev") {
                self.refuse(format!(
                    "{} opened as {:?}",
                    path.display(),
                    call.returned_path
                ));
            }
            return;
        }
        let opened = call
            .returned_path
            .as_ref()
            .map_or_else(|| path.to_path_buf(), PathBuf::from);
        if writes {
            // Fresh contents only when the call guarantees them: a new file
            // (O_EXCL, O_TMPFILE) or one emptied (O_TRUNC).
            if flag("O_EXCL") || flag("O_TMPFILE") || flag("O_TRUNC") {
                self.made(&opened);
            }
            self.write(&opened);
        } else if call.arguments.contains("O_DIRECTORY") || opened.is_dir() {
            self.read(&opened, Input::Directory(opened.clone()));
        } else {
            self.read(&opened, Input::File(opened.clone()));
        }
    }

    /// A call that changes the file system, or tried to: `paths` are its
    /// string arguments as paths.
    fn change(&mut self, call: &Call, paths: &[PathBuf]) {
        // A symbolic link's first string is what it points to, which is
        // text, not a path written.
        let name = call.name.as_str();
        let first = usize::from(matches!(name, "symlink" | "symlinkat"));
        for path in paths.iter().skip(first) {
            if call.returned < 0 {
                // A change that did not happen -- a mkdir of a directory
                // already there -- only looked the path up.
                self.absent(path, call);
            } else {
                self.write(path);
            }
        }
        // What a successful call makes new: a directory, a node, and the
        // target of a rename or a link.
        if call.returned >= 0 {
            let fresh = match name {
                "mkdir" | "mkdirat" | "mknod" | "mknodat" | "rename" | "renameat" | "renameat2"
                | "link" | "linkat" | "symlink" | "symlinkat" => paths.last(),
                _ => None,
            };
            if let Some(path) = fresh {
                self.made(path);
            }
        }
    }

    /// The path the roots are judged by: where it leads now, or, gone,
    /// with `..` taken away lexically, so that `..` or a link cannot carry
    /// a path out of a root it seems to be in.
    fn real(path: &Path) -> PathBuf {
        let lexical = lexical(path);
        // /proc's links name the process that looks, which is not the
        // unit's but this one: they are judged as written.
        if lexical.starts_with("/proc") || lexical.starts_with("/dev/fd") {
            return lexical;
        }
        std::fs::canonicalize(path).unwrap_or(lexical)
    }

    /// A path read: recorded, unless excluded; refused when it is the
    /// item's, a temporary file the run had not made yet, or another
    /// step's outputs.
    fn read(&mut self, path: &Path, input: Input) {
        if !self.judging {
            return;
        }
        if not_utf8(&mut self.traced, path) {
            return;
        }
        let real = Self::real(path);
        if let Some(item) = self
            .context
            .item_path(&real)
            .or_else(|| self.context.item_path(path))
        {
            self.traced
                .refuse(format!("it read the certified item ({item})"));
            return;
        }
        if self.context.is_temporary(&real) {
            if !self.context.is_temporary_root(&real)
                && !self.made_before_now(path)
                && !self.made_before_now(&real)
            {
                self.traced
                    .refuse(format!("it read {}, which it had not made", path.display()));
            }
            return;
        }
        if self.context.is_excluded(&real) {
            return;
        }
        if real.starts_with(&self.context.target) {
            // xtask itself, in the prelude; and what the run made.
            if real == self.context.xtask
                || self.made_before_now(path)
                || self.made_before_now(&real)
            {
                return;
            }
            // A lookup there -- the dynamic loader's search of the
            // directories cargo puts on LD_LIBRARY_PATH -- is an input like
            // any other; reading another step's outputs is not recorded.
            if !matches!(input, Input::Probe(..)) {
                self.traced
                    .refuse(format!("it read {}, another step's output", path.display()));
                return;
            }
        }
        let _ = self.traced.inputs.insert(input);
    }

    /// A path looked up.
    fn probe(&mut self, path: &Path, call: &Call, follow: bool) {
        if call.returned < 0 {
            self.absent(path, call);
        } else {
            self.read(path, Input::Probe(path.to_path_buf(), follow));
        }
    }

    /// A lookup that failed: the path, absent, is an input as much as a
    /// file.
    fn absent(&mut self, path: &Path, call: &Call) {
        if !self.judging || not_utf8(&mut self.traced, path) {
            return;
        }
        let real = Self::real(path);
        if self.context.is_temporary(&real) || self.context.is_excluded(&real) {
            return;
        }
        if let Some(item) = self
            .context
            .item_path(&real)
            .or_else(|| self.context.item_path(path))
        {
            self.traced.refuse(format!(
                "it looked up a path of the certified item ({item})"
            ));
            return;
        }
        match call.error.as_deref() {
            // Absent, or failures that say nothing more than the path's
            // state does, which the record keeps: a call the kernel lacks,
            // an interrupted one, an executable bit looked for, readlink's
            // EINVAL on what is no link, a mkdir of what exists.
            Some(
                "ENOENT" | "ENOTDIR" | "ENOSYS" | "EINTR" | "EAGAIN" | "EACCES" | "EPERM" | "ENXIO"
                | "ENODATA" | "EOPNOTSUPP" | "ENOEXEC" | "ELOOP" | "EISDIR" | "EINVAL" | "EEXIST"
                | "ENOTEMPTY" | "EXDEV" | "EBUSY",
            ) => {
                let _ = self.traced.inputs.insert(Input::Probe(
                    path.to_path_buf(),
                    !call.arguments.contains("AT_SYMLINK_NOFOLLOW"),
                ));
            }
            other => self.traced.refuse(format!(
                "{} on {} failed with {}",
                call.name,
                path.display(),
                other.unwrap_or("no error name")
            )),
        }
    }

    /// A path written: allowed in the target directory, the temporary
    /// directories and `/dev` (and the excluded caches), refused anywhere
    /// else.
    fn write(&mut self, path: &Path) {
        if !self.judging || not_utf8(&mut self.traced, path) {
            return;
        }
        let real = Self::real(path);
        if self.context.is_temporary(&real)
            || self.context.is_excluded(&real)
            || real.starts_with(&self.context.target)
        {
            return;
        }
        self.traced.refuse(format!("it wrote {}", path.display()));
    }
}

/// `path` with `.` and `..` taken away lexically.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                let _ = out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The Unix socket address in a call's arguments: `sun_path="/run/x"`, or
/// an abstract one, `sun_path=@"name"`.
fn unix_address(arguments: &str) -> Option<String> {
    if !arguments.contains("AF_UNIX") {
        return None;
    }
    let (_, rest) = arguments.split_once("sun_path=")?;
    let (abstract_name, rest) = match rest.strip_prefix('@') {
        Some(rest) => ("@", rest),
        None => ("", rest),
    };
    let rest = rest.strip_prefix('"')?;
    let (text, _) = unquote(rest.as_bytes(), 0).ok()?;
    Some(format!("{abstract_name}{text}"))
}

/// Whether `path` came from a string that is not UTF-8, refusing the
/// record if so.
fn not_utf8(traced: &mut Traced, path: &Path) -> bool {
    if path.to_string_lossy().contains('\0') {
        traced.refuse(format!("a path that is not UTF-8: {}", path.display()));
        return true;
    }
    false
}

/// The programs the kernel starts `program` with without the program
/// opening them: a `#!` line's interpreter, and an ELF's `PT_INTERP`.
fn interpreters(program: &Path) -> Vec<PathBuf> {
    let Ok(bytes) = std::fs::read(program) else {
        return Vec::new();
    };
    if let Some(rest) = bytes.strip_prefix(b"#!") {
        let line = rest.split(|b| *b == b'\n').next().unwrap_or_default();
        let line = String::from_utf8_lossy(line);
        return line
            .split_whitespace()
            .next()
            .map(|interpreter| vec![PathBuf::from(interpreter)])
            .unwrap_or_default();
    }
    elf_interpreter(&bytes).into_iter().collect()
}

/// A 64-bit little-endian ELF's `PT_INTERP`.
fn elf_interpreter(bytes: &[u8]) -> Option<PathBuf> {
    if bytes.get(..6)? != b"\x7fELF\x02\x01" {
        return None;
    }
    let u16_at = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    let u64_at = |at: usize| Some(u64::from_le_bytes(bytes.get(at..at + 8)?.try_into().ok()?));
    let table = usize::try_from(u64_at(0x20)?).ok()?;
    let entry = usize::from(u16_at(0x36)?);
    let count = usize::from(u16_at(0x38)?);
    for index in 0..count {
        let header = table + index * entry;
        if u32_at(header)? == 3 {
            let offset = usize::try_from(u64_at(header + 8)?).ok()?;
            let size = usize::try_from(u64_at(header + 32)?).ok()?;
            let text = bytes.get(offset..offset + size)?;
            let text = text.split(|b| *b == 0).next()?;
            return Some(PathBuf::from(String::from_utf8_lossy(text).into_owned()));
        }
    }
    None
}

// -- records ---------------------------------------------------------------

/// The text of a record of the unit traced in `traces`, or why there is
/// none.
fn record(name: &str, traces: &Path, context: &Context) -> std::result::Result<String, String> {
    let traced = read_traces(traces, &context.root, context)?;
    if let Some(why) = traced.refused {
        return Err(why);
    }
    // The toolchains the unit ran, which the excluded roots leave out.
    let mut toolchains = BTreeSet::new();
    for input in &traced.inputs {
        let path = input_path(input);
        if let Some(toolchain) = context.toolchain_of(path) {
            let _ = toolchains.insert(toolchain);
        }
    }
    for line in std::fs::read_dir(traces)
        .map_err(|error| error.to_string())?
        .flatten()
    {
        let text = std::fs::read_to_string(line.path()).unwrap_or_default();
        for word in text.split('"') {
            if let Some(toolchain) = context.toolchain_of(Path::new(word)) {
                let _ = toolchains.insert(toolchain);
            }
        }
    }
    let mut text = format!("{MAGIC}\nunit {name}\norigin {}\n", context.origin);
    for line in &context.prelude {
        text.push_str(line);
        text.push('\n');
    }
    for toolchain in &toolchains {
        text.push_str(&format!(
            "toolchain {} {}\n",
            toolchain_digest(toolchain)?,
            toolchain.display()
        ));
    }
    for (root, why) in EXCLUDED {
        text.push_str(&format!("excluded {root}: {why}\n"));
    }
    for input in &traced.inputs {
        let state = state_of(input, context)?;
        let written = context.written(input_path(input));
        if written.contains('\n') {
            return Err("a path with a line break".to_owned());
        }
        text.push_str(&format!("{state} {written}\n"));
    }
    Ok(text)
}

/// The path an input names.
fn input_path(input: &Input) -> &Path {
    match input {
        Input::File(path) | Input::Directory(path) | Input::Probe(path, _) | Input::Link(path) => {
            path
        }
    }
}

/// An input's state now, as a record's line starts: `F <digest>`,
/// `D <digest>`, `P<f|n> <what>`, `L <digest>`.
fn state_of(input: &Input, context: &Context) -> std::result::Result<String, String> {
    match input {
        Input::File(path) => Ok(format!("F {}", file_digest(path)?)),
        Input::Directory(path) => {
            let mut names = Vec::new();
            for entry in
                std::fs::read_dir(path).map_err(|error| format!("{}: {error}", path.display()))?
            {
                let entry = entry.map_err(|error| error.to_string())?;
                // An entry under an excluded root (cargo's lock files beside
                // ~/.cargo's registry) is no input, and comes and goes.
                if context.is_excluded(&entry.path()) {
                    continue;
                }
                let kind = entry.file_type().map_err(|error| error.to_string())?;
                names.push(format!(
                    "{} {}",
                    entry.file_name().to_string_lossy(),
                    kind_letter(kind)
                ));
            }
            names.sort();
            Ok(format!("D {}", sha256::hex(names.join("\n").as_bytes())))
        }
        Input::Probe(path, follow) => {
            let metadata = if *follow {
                std::fs::metadata(path)
            } else {
                std::fs::symlink_metadata(path)
            };
            let what = match metadata {
                // A file looked up is kept by its bytes, not its size: cargo
                // stats a warm build's sources and never opens them, and an
                // edit can keep a size (the consultant's B1, ledger 1494).
                Ok(metadata) if metadata.is_file() => format!("file:{}", file_digest(path)?),
                Ok(metadata) => kind_letter(metadata.file_type()).to_owned(),
                Err(error) => match error.raw_os_error() {
                    Some(code) => format!("error:{code}"),
                    None => return Err(format!("{}: {error}", path.display())),
                },
            };
            Ok(format!("P{} {what}", if *follow { "f" } else { "n" }))
        }
        Input::Link(path) => {
            let target =
                std::fs::read_link(path).map_err(|error| format!("{}: {error}", path.display()))?;
            Ok(format!(
                "L {}",
                sha256::hex(target.to_string_lossy().as_bytes())
            ))
        }
    }
}

/// A word for a kind of file.
fn kind_letter(kind: std::fs::FileType) -> &'static str {
    #[cfg(unix)]
    use std::os::unix::fs::FileTypeExt as _;
    if kind.is_dir() {
        return "dir";
    }
    if kind.is_symlink() {
        return "link";
    }
    #[cfg(unix)]
    if kind.is_fifo() || kind.is_socket() || kind.is_char_device() || kind.is_block_device() {
        return "other";
    }
    "file"
}

/// Digests already taken in this process, by path and the file's identity
/// and times: a library every unit loads is read once per `check`, and a
/// file written since (whose change time the kernel moves) is read again.
static DIGESTS: Mutex<Option<HashMap<(PathBuf, String), String>>> = Mutex::new(None);

/// The SHA-256 of the file at `path`.
fn file_digest(path: &Path) -> std::result::Result<String, String> {
    let identity = identity(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let key = (path.to_path_buf(), identity);
    {
        let mut digests = DIGESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(digest) = digests.get_or_insert_with(HashMap::new).get(&key) {
            return Ok(digest.clone());
        }
    }
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let digest = sha256::hex(&bytes);
    let mut digests = DIGESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = digests
        .get_or_insert_with(HashMap::new)
        .insert(key, digest.clone());
    Ok(digest)
}

/// A file's device, inode, size and modification and change times.
#[cfg(unix)]
fn identity(path: &Path) -> std::io::Result<String> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = std::fs::metadata(path)?;
    Ok(format!(
        "{} {} {} {}.{} {}.{}",
        metadata.dev(),
        metadata.ino(),
        metadata.size(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec()
    ))
}

/// Elsewhere, no identity: every digest is taken again.
#[cfg(not(unix))]
fn identity(_: &Path) -> std::io::Result<String> {
    Ok(format!("{:?}", std::time::Instant::now()))
}

/// Toolchain digests already taken in this process.
static TOOLCHAINS: Mutex<Option<HashMap<PathBuf, String>>> = Mutex::new(None);

/// [`toolchain_digest_now`], once per toolchain in a process: a toolchain
/// does not change in the minutes a `check` runs, and the versions are four
/// programs started.
fn toolchain_digest(toolchain: &Path) -> std::result::Result<String, String> {
    if !cfg!(test)
        && let Some(digest) = TOOLCHAINS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .get(toolchain)
    {
        return Ok(digest.clone());
    }
    let digest = toolchain_digest_now(toolchain)?;
    let _ = TOOLCHAINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .insert(toolchain.to_path_buf(), digest.clone());
    Ok(digest)
}

/// A toolchain's version strings and installed components.
fn toolchain_digest_now(toolchain: &Path) -> std::result::Result<String, String> {
    let mut text = String::new();
    for (program, argument) in [
        ("rustc", "-vV"),
        ("cargo", "-V"),
        ("clippy-driver", "-V"),
        ("rustfmt", "-V"),
    ] {
        let binary = toolchain.join("bin").join(program);
        if !binary.exists() {
            text.push_str(&format!("{program}: none\n"));
            continue;
        }
        let output = Command::new(&binary)
            .arg(argument)
            .output()
            .map_err(|error| format!("{}: {error}", binary.display()))?;
        text.push_str(&format!(
            "{program}: {}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let components = toolchain.join("lib/rustlib/components");
    text.push_str(
        &std::fs::read_to_string(&components).unwrap_or_else(|_| "no components file\n".to_owned()),
    );
    Ok(sha256::hex(text.as_bytes()))
}

/// Write a record into `directory`, atomically, and keep the newest
/// [`KEPT`].
fn write_record(directory: &Path, text: &str) -> Result<()> {
    let name = format!("{}.rec", sha256::hex(text.as_bytes()));
    let temporary = directory.join(format!(".{name}.{}", std::process::id()));
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, directory.join(name))?;
    let mut records = records(directory);
    while records.len() > KEPT {
        if let Some((_, oldest)) = records.pop() {
            let _ = std::fs::remove_file(oldest);
        }
    }
    Ok(())
}

/// The records in `directory`, newest first.
fn records(directory: &Path) -> Vec<(SystemTime, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rec"))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()?;
            Some((modified, path))
        })
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found
}

/// The origin and input count of the first record in `directory` whose
/// every line holds now.
fn lookup(name: &str, directory: &Path, context: &Context) -> Option<(String, usize)> {
    for (modified, path) in records(directory) {
        if SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            > EXPIRY
        {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(found) = holds(name, &text, context) {
            return Some(found);
        }
    }
    None
}

/// Whether every line of the record `text` holds now: its origin and how
/// many inputs it checked, or `None`.
pub(crate) fn holds(name: &str, text: &str, context: &Context) -> Option<(String, usize)> {
    let mut lines = text.lines();
    if lines.next()? != MAGIC {
        return None;
    }
    if lines.next()?.strip_prefix("unit ")? != name {
        return None;
    }
    let origin = lines.next()?.strip_prefix("origin ")?.to_owned();
    for expected in &context.prelude {
        if lines.next()? != expected {
            return None;
        }
    }
    let mut inputs = 0;
    for line in lines {
        let (kind, rest) = line.split_once(' ')?;
        match kind {
            "toolchain" => {
                let (digest, toolchain) = rest.split_once(' ')?;
                if toolchain_digest(Path::new(toolchain)).ok()? != digest {
                    return None;
                }
            }
            "excluded" => {
                if !EXCLUDED
                    .iter()
                    .any(|(root, why)| rest == format!("{root}: {why}"))
                {
                    return None;
                }
            }
            "F" | "D" | "L" | "Pf" | "Pn" => {
                let (state, written) = rest.split_once(' ')?;
                let path = context.read_back(written);
                let input = match kind {
                    "F" => Input::File(path),
                    "D" => Input::Directory(path),
                    "L" => Input::Link(path),
                    "Pf" => Input::Probe(path, true),
                    _ => Input::Probe(path, false),
                };
                if state_of(&input, context).ok()? != format!("{kind} {state}") {
                    return None;
                }
                inputs += 1;
            }
            _ => return None,
        }
    }
    Some((origin, inputs))
}

#[cfg(test)]
mod tests;
