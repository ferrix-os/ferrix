//! Builds that can be made on another machine: stage 20's record, carry out
//! and replay.
//!
//! Every program xtask boots is compiled by one of the [`Build`]s here -- a
//! `cargo build` in a directory of the tree, with some environment and the
//! files it names -- and the rest of xtask only packs and boots what they
//! produce. So "Ferrix's images, compiled on Ferrix" is three steps:
//!
//! 1. **Record.** With `FERRIX_BUILDS=record:<DIR>`, xtask builds as usual and
//!    also writes each build down in `<DIR>/plan`: its command, its
//!    environment, the files it reads, and the digests of what it made. A file
//!    it reads that an earlier build made is written down as that build's
//!    output; any other is copied into `<DIR>/files`. Paths inside the tree,
//!    the target directory and `~/.local/share/ferrix` are written as
//!    `${ROOT}`, `${TARGET}` and `${DATA}`.
//! 2. **Carry out.** `cargo xtask builds-execute --plan <DIR>` makes every
//!    build in the plan, in order, with this machine's compiler -- on Ferrix,
//!    under `test-selfhost --plan` -- and keeps each one's outputs in
//!    `<DIR>/store/<key>/`. A build that reads an earlier build's output reads
//!    the one made here.
//! 3. **Replay.** With `FERRIX_BUILDS=replay:<DIR>/store`, xtask makes no
//!    build at all: each is answered from the store, and a build the store
//!    does not have is an error that names it. Every test then boots what
//!    Ferrix compiled.
//!
//! A build's key is the SHA-256 of what it is -- the command, the
//! environment, the paths with the placeholders in them -- and of the bytes
//! of every file it reads. Step 2 and step 3 compute it from the same
//! things: in both, an input made by an earlier build is Ferrix's.
//!
//! A script build also reads files at places of its own rather than through
//! a variable -- the source archives it would otherwise download -- and names
//! the directory with [`Build::reads_dir`]. Its files are listed and hashed
//! when the build is recorded and when it is replayed, and put in place
//! before it is carried out.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{Error, Result, paths, sha256};

/// The variable that switches between building, recording and replaying.
const VARIABLE: &str = "FERRIX_BUILDS";

/// What a build runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Program {
    /// Cargo: the one this xtask was run by.
    Cargo,
    /// `bash`, for the builds that are scripts.
    Bash,
}

impl Program {
    fn name(self) -> &'static str {
        match self {
            Program::Cargo => "cargo",
            Program::Bash => "bash",
        }
    }

    fn named(name: &str) -> Option<Program> {
        match name {
            "cargo" => Some(Program::Cargo),
            "bash" => Some(Program::Bash),
            _ => None,
        }
    }

    fn command(self) -> Command {
        match self {
            Program::Cargo => Command::new(crate::cargo::cargo()),
            Program::Bash => Command::new("bash"),
        }
    }
}

/// One build: a command in a directory of the tree, and the files it makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Build {
    /// What it is, for the console and for errors.
    what: String,
    program: Program,
    /// Where it runs.
    dir: PathBuf,
    args: Vec<String>,
    /// Environment set on top of what the command inherits.
    env: Vec<(String, String)>,
    /// Environment variables that name a file the build reads, and the file.
    inputs: Vec<(String, PathBuf)>,
    /// Directories whose files the build reads where they are.
    reads: Vec<PathBuf>,
    /// The files the caller reads once the build is made.
    outputs: Vec<PathBuf>,
}

impl Build {
    /// A `cargo` build in `dir`.
    pub(crate) fn cargo(what: impl Into<String>, dir: impl Into<PathBuf>) -> Build {
        Build::new(Program::Cargo, what, dir)
    }

    /// A `bash` build in `dir`.
    pub(crate) fn bash(what: impl Into<String>, dir: impl Into<PathBuf>) -> Build {
        Build::new(Program::Bash, what, dir)
    }

    fn new(program: Program, what: impl Into<String>, dir: impl Into<PathBuf>) -> Build {
        Build {
            what: what.into(),
            program,
            dir: dir.into(),
            args: Vec::new(),
            env: Vec::new(),
            inputs: Vec::new(),
            reads: Vec::new(),
            outputs: Vec::new(),
        }
    }

    /// Arguments, in order.
    #[must_use]
    pub(crate) fn args<I, S>(mut self, args: I) -> Build
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|arg| text(arg.as_ref())));
        self
    }

    /// An environment variable.
    #[must_use]
    pub(crate) fn env(mut self, key: &str, value: impl AsRef<OsStr>) -> Build {
        self.env.push((key.to_owned(), text(value.as_ref())));
        self
    }

    /// An environment variable naming a file the build reads.
    #[must_use]
    pub(crate) fn input(mut self, key: &str, file: impl Into<PathBuf>) -> Build {
        self.inputs.push((key.to_owned(), file.into()));
        self
    }

    /// A directory whose files the build reads where they are.
    #[must_use]
    pub(crate) fn reads_dir(mut self, directory: impl Into<PathBuf>) -> Build {
        self.reads.push(directory.into());
        self
    }

    /// A file the build makes that the caller will read, or a directory,
    /// which is kept, compared and put back whole: its links as links, its
    /// files with their modes.
    #[must_use]
    pub(crate) fn output(mut self, file: impl Into<PathBuf>) -> Build {
        self.outputs.push(file.into());
        self
    }

    /// Make it, record it, or take it from the store, as [`VARIABLE`] says.
    ///
    /// # Errors
    ///
    /// When the command fails, the plan cannot be written, or the store does
    /// not have the build.
    pub(crate) fn run(self) -> Result<()> {
        match Mode::from_environment()? {
            Mode::Build => self.make(),
            Mode::Record(directory) | Mode::Plan(directory) => {
                self.make()?;
                record(&directory, &self)
            }
            Mode::Replay(store) => replay(&store, &self),
        }
    }

    /// Run the command here.
    fn make(&self) -> Result<()> {
        let mut command = self.program.command();
        let _ = command.current_dir(&self.dir).args(&self.args);
        for (key, value) in &self.env {
            let _ = command.env(key, value);
        }
        for (key, file) in &self.inputs {
            let _ = command.env(key, file);
            // And what the file holds, for a build script to declare with
            // `rerun-if-env-changed`. Cargo judges a `rerun-if-changed` file
            // by its mtime against the script's last run, and an input's mtime
            // can go backwards: cargo hands back an older build's artifact,
            // with that build's mtime, when a feature is switched back. A
            // variable cargo compares by value cannot be fooled that way.
            let _ = command.env(digest_variable(key), digest_of(file)?);
        }
        crate::cargo::run(command, &self.what)
    }
}

/// The variable [`Build::make`] sets beside the input `key`: the SHA-256 of
/// the file's bytes, in hex. Not part of a build's key, which hashes the
/// bytes already.
pub(crate) fn digest_variable(key: &str) -> String {
    format!("{key}_DIGEST")
}

/// Whether builds are being recorded or replayed, so that a caller that
/// would skip a build it thinks current makes it anyway: a build that does
/// not run is neither written down nor answered from the store.
pub(crate) fn active() -> bool {
    matches!(
        Mode::from_environment(),
        Ok(Mode::Record(_) | Mode::Plan(_) | Mode::Replay(_))
    )
}

/// Whether builds are recorded without booting what they made
/// (`FERRIX_BUILDS=plan:<DIR>`): a plan written in minutes and with no
/// machine to boot, as a scheduled CI job makes one. `crate::qemu` refuses
/// every boot then, and a test stops at its first.
pub(crate) fn boots_skipped() -> bool {
    matches!(Mode::from_environment(), Ok(Mode::Plan(_)))
}

/// An error in place of a boot when [`boots_skipped`], for `crate::qemu` to
/// return before it starts a machine.
///
/// # Errors
///
/// When boots are skipped.
pub(crate) fn refuse_boot(arch: impl std::fmt::Display) -> Result<()> {
    if boots_skipped() {
        return Err(Error::new(format!(
            "{arch}: not booted: FERRIX_BUILDS=plan: records the builds and boots nothing"
        )));
    }
    Ok(())
}

/// Every file in the directories `build` reads, sorted, with its digest.
fn files_read(build: &Build) -> Result<Vec<(PathBuf, String)>> {
    let mut files = Vec::new();
    for directory in &build.reads {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.is_file() {
                let digest = digest_of(&path)?;
                files.push((path, digest));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// A path or argument as text: every one xtask writes is UTF-8.
fn text(value: &OsStr) -> String {
    value.to_string_lossy().into_owned()
}

/// Building, recording or replaying.
#[derive(Debug, PartialEq, Eq)]
enum Mode {
    Build,
    Record(PathBuf),
    /// [`Mode::Record`] with every boot refused: see [`boots_skipped`].
    Plan(PathBuf),
    Replay(PathBuf),
}

impl Mode {
    fn from_environment() -> Result<Mode> {
        match std::env::var(VARIABLE) {
            Err(_) => Ok(Mode::Build),
            Ok(value) => Mode::parse(&value),
        }
    }

    fn parse(value: &str) -> Result<Mode> {
        if value.is_empty() {
            Ok(Mode::Build)
        } else if let Some(directory) = value.strip_prefix("record:") {
            Ok(Mode::Record(PathBuf::from(directory)))
        } else if let Some(directory) = value.strip_prefix("plan:") {
            Ok(Mode::Plan(PathBuf::from(directory)))
        } else if let Some(directory) = value.strip_prefix("replay:") {
            Ok(Mode::Replay(PathBuf::from(directory)))
        } else {
            Err(Error::new(format!(
                "{VARIABLE}={value} is none of record:<DIR>, plan:<DIR> and replay:<DIR>"
            )))
        }
    }
}

/// Where this machine's tree, target directory and data directory are, for
/// [`Places::portable`] and [`Places::local`].
struct Places {
    root: String,
    target: String,
    data: String,
}

impl Places {
    fn here() -> Places {
        let data = std::env::home_dir()
            .map(|home| text(home.join(".local/share/ferrix").as_os_str()))
            .unwrap_or_default();
        Places {
            root: text(paths::workspace_root().as_os_str()),
            target: text(paths::target_dir().as_os_str()),
            data,
        }
    }

    /// `value` with this machine's places written as placeholders. The target
    /// directory first, since it is often inside one of the other two.
    fn portable(&self, value: &str) -> String {
        let mut value = value.replace(&self.target, "${TARGET}");
        if !self.data.is_empty() {
            value = value.replace(&self.data, "${DATA}");
        }
        value.replace(&self.root, "${ROOT}")
    }

    /// The other way.
    fn local(&self, value: &str) -> String {
        value
            .replace("${TARGET}", &self.target)
            .replace("${DATA}", &self.data)
            .replace("${ROOT}", &self.root)
    }
}

/// The key of `build`: what it is, with its places portable and its inputs
/// by content.
fn key(build: &Build, places: &Places) -> Result<String> {
    let mut description = format!(
        "{}\ndir {}\n",
        build.program.name(),
        places.portable(&text(build.dir.as_os_str()))
    );
    for arg in &build.args {
        description.push_str(&format!("arg {}\n", places.portable(arg)));
    }
    let env: BTreeMap<&str, String> = build
        .env
        .iter()
        .map(|(key, value)| (key.as_str(), places.portable(value)))
        .collect();
    for (key, value) in env {
        description.push_str(&format!("env {key}={value}\n"));
    }
    let inputs: BTreeMap<&str, &Path> = build
        .inputs
        .iter()
        .map(|(key, file)| (key.as_str(), file.as_path()))
        .collect();
    for (key, file) in inputs {
        description.push_str(&format!("input {key}={}\n", digest_of(file)?));
    }
    for (file, digest) in files_read(build)? {
        description.push_str(&format!(
            "read {}={digest}\n",
            places.portable(&text(file.as_os_str()))
        ));
    }
    for output in &build.outputs {
        description.push_str(&format!(
            "output {}\n",
            places.portable(&text(output.as_os_str()))
        ));
    }
    Ok(sha256::hex(description.as_bytes()))
}

/// The SHA-256 of what an output is: a file's bytes, or for a directory the
/// name, kind, mode and digest of everything in it, in name order.
fn digest_of_output(path: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|error| Error::new(format!("reading {}: {error}", path.display())))?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        return Ok(sha256::hex(
            format!("link {}", text(target.as_os_str())).as_bytes(),
        ));
    }
    if !meta.is_dir() {
        return digest_of(path);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    let mut listing = String::new();
    for entry in entries {
        let name = entry.file_name().map(text).unwrap_or_default();
        listing.push_str(&format!(
            "{name} {:o} {}\n",
            mode_of(&entry),
            digest_of_output(&entry)?
        ));
    }
    Ok(sha256::hex(listing.as_bytes()))
}

/// A file's permission bits, or zero where there are none to read.
fn mode_of(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::symlink_metadata(path).map_or(0, |meta| meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
}

/// Copy `from`, a file, a link or a directory, to `to`, replacing whatever is
/// there.
fn copy_output(from: &Path, to: &Path) -> Result<()> {
    let failed = |error: std::io::Error| {
        Error::new(format!(
            "copying {} to {}: {error}",
            from.display(),
            to.display()
        ))
    };
    if let Ok(meta) = std::fs::symlink_metadata(to) {
        if meta.is_dir() {
            std::fs::remove_dir_all(to).map_err(failed)?;
        } else {
            std::fs::remove_file(to).map_err(failed)?;
        }
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(failed)?;
    }
    let meta = std::fs::symlink_metadata(from).map_err(failed)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(from).map_err(failed)?;
        return link(&target, to).map_err(failed);
    }
    if meta.is_dir() {
        std::fs::create_dir_all(to).map_err(failed)?;
        for entry in std::fs::read_dir(from).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            copy_output(&entry.path(), &to.join(entry.file_name()))?;
        }
        return Ok(());
    }
    let _ = std::fs::copy(from, to).map_err(failed)?;
    Ok(())
}

/// A symbolic link at `to` reading `target`.
#[cfg(unix)]
fn link(target: &Path, to: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, to)
}

/// A symbolic link, which only a Unix host makes: recording and replaying are
/// for the Linux host that makes and boots the images.
#[cfg(not(unix))]
fn link(_target: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(
        "a symbolic link cannot be made on this host",
    ))
}

/// The SHA-256 of a file's bytes.
fn digest_of(file: &Path) -> Result<String> {
    let bytes = std::fs::read(file)
        .map_err(|error| Error::new(format!("reading {}: {error}", file.display())))?;
    Ok(sha256::hex(&bytes))
}

/// Percent-escape `value` so it is one space-free token on one line.
fn escape(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    for byte in value.bytes() {
        if byte <= b' ' || byte == b'%' || byte == 0x7f {
            out.extend_from_slice(format!("%{byte:02X}").as_bytes());
        } else {
            out.push(byte);
        }
    }
    // Only ASCII bytes were replaced, by ASCII, so a UTF-8 sequence is
    // copied whole and the result is UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// [`escape`], undone.
fn unescape(token: &str) -> Result<String> {
    let mut bytes = Vec::with_capacity(token.len());
    let mut rest = token.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'%' {
            let digits: String = rest.by_ref().take(2).map(char::from).collect();
            let value = u8::from_str_radix(&digits, 16)
                .map_err(|_| Error::new(format!("a bad escape in the plan: {token}")))?;
            bytes.push(value);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).map_err(|_| Error::new(format!("a plan token not UTF-8: {token}")))
}

/// Where a planned build's input comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// The output with this digest of an earlier build in the plan.
    Output(String),
    /// A file carried in the plan's `files/`, by its digest.
    File(String),
}

/// A build as the plan has it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Planned {
    /// The key it had where it was recorded.
    key: String,
    what: String,
    program: Program,
    dir: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    inputs: Vec<(String, Source)>,
    /// Each directory read, portable.
    reads: Vec<String>,
    /// Each file read in them, portable, by the digest it is carried under.
    read_files: Vec<(String, String)>,
    /// Each output, portable, with the digest it had where recorded.
    outputs: Vec<(String, String)>,
}

impl Planned {
    fn write(&self) -> String {
        let mut out = format!("build {}\nwhat {}\n", self.key, escape(&self.what));
        out.push_str(&format!(
            "program {}\ndir {}\n",
            self.program.name(),
            escape(&self.dir)
        ));
        for arg in &self.args {
            out.push_str(&format!("arg {}\n", escape(arg)));
        }
        for (key, value) in &self.env {
            out.push_str(&format!("env {} {}\n", escape(key), escape(value)));
        }
        for (key, source) in &self.inputs {
            let (kind, digest) = match source {
                Source::Output(digest) => ("output", digest),
                Source::File(digest) => ("file", digest),
            };
            out.push_str(&format!("input {} {kind} {digest}\n", escape(key)));
        }
        for directory in &self.reads {
            out.push_str(&format!("reads {}\n", escape(directory)));
        }
        for (file, digest) in &self.read_files {
            out.push_str(&format!("read {} {digest}\n", escape(file)));
        }
        for (file, digest) in &self.outputs {
            out.push_str(&format!("output {} {digest}\n", escape(file)));
        }
        out.push_str("end\n");
        out
    }
}

/// Every build in a plan, in order.
fn parse(plan: &str) -> Result<Vec<Planned>> {
    let bad = |line: &str| Error::new(format!("a line the plan should not have: {line}"));
    let mut builds = Vec::new();
    let mut current: Option<Planned> = None;
    for line in plan.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<&str> = line.split(' ').collect();
        match (fields.as_slice(), current.as_mut()) {
            (["build", key], None) => {
                current = Some(Planned {
                    key: (*key).to_owned(),
                    what: String::new(),
                    program: Program::Cargo,
                    dir: String::new(),
                    args: Vec::new(),
                    env: Vec::new(),
                    inputs: Vec::new(),
                    reads: Vec::new(),
                    read_files: Vec::new(),
                    outputs: Vec::new(),
                });
            }
            (["what", what], Some(build)) => build.what = unescape(what)?,
            (["program", name], Some(build)) => {
                build.program = Program::named(name).ok_or_else(|| bad(line))?;
            }
            (["dir", dir], Some(build)) => build.dir = unescape(dir)?,
            (["arg", arg], Some(build)) => build.args.push(unescape(arg)?),
            (["env", key, value], Some(build)) => {
                build.env.push((unescape(key)?, unescape(value)?));
            }
            (["input", key, "output", digest], Some(build)) => build
                .inputs
                .push((unescape(key)?, Source::Output((*digest).to_owned()))),
            (["input", key, "file", digest], Some(build)) => build
                .inputs
                .push((unescape(key)?, Source::File((*digest).to_owned()))),
            (["reads", directory], Some(build)) => build.reads.push(unescape(directory)?),
            (["read", file, digest], Some(build)) => build
                .read_files
                .push((unescape(file)?, (*digest).to_owned())),
            (["output", file, digest], Some(build)) => {
                build.outputs.push((unescape(file)?, (*digest).to_owned()));
            }
            (["end"], Some(_)) => builds.extend(current.take()),
            _ => return Err(bad(line)),
        }
    }
    if current.is_some() {
        return Err(Error::new("the plan ends inside a build"));
    }
    Ok(builds)
}

/// Write `build`, just made, into the plan in `directory`, unless the plan
/// has it already.
fn record(directory: &Path, build: &Build) -> Result<()> {
    let places = Places::here();
    let key = key(build, &places)?;
    let plan_file = directory.join("plan");
    let existing = std::fs::read_to_string(&plan_file).unwrap_or_default();
    let planned = parse(&existing)?;
    if planned.iter().any(|earlier| earlier.key == key) {
        return Ok(());
    }
    let made: BTreeSet<&str> = planned
        .iter()
        .flat_map(|earlier| earlier.outputs.iter().map(|(_, digest)| digest.as_str()))
        .collect();
    let files = directory.join("files");
    std::fs::create_dir_all(&files)?;
    let mut inputs = Vec::new();
    for (name, file) in &build.inputs {
        let digest = digest_of(file)?;
        if made.contains(digest.as_str()) {
            inputs.push((name.clone(), Source::Output(digest)));
        } else {
            let carried = files.join(&digest);
            if !carried.is_file() {
                let _ = std::fs::copy(file, &carried).map_err(|error| {
                    Error::new(format!("copying {} into the plan: {error}", file.display()))
                })?;
            }
            inputs.push((name.clone(), Source::File(digest)));
        }
    }
    let mut read_files = Vec::new();
    for (file, digest) in files_read(build)? {
        let carried = files.join(&digest);
        if !carried.is_file() {
            let _ = std::fs::copy(&file, &carried).map_err(|error| {
                Error::new(format!("copying {} into the plan: {error}", file.display()))
            })?;
        }
        read_files.push((places.portable(&text(file.as_os_str())), digest));
    }
    let mut outputs = Vec::new();
    for output in &build.outputs {
        outputs.push((
            places.portable(&text(output.as_os_str())),
            digest_of_output(output)?,
        ));
    }
    let entry = Planned {
        key,
        what: build.what.clone(),
        program: build.program,
        dir: places.portable(&text(build.dir.as_os_str())),
        args: build.args.iter().map(|arg| places.portable(arg)).collect(),
        env: build
            .env
            .iter()
            .map(|(name, value)| (name.clone(), places.portable(value)))
            .collect(),
        inputs,
        reads: build
            .reads
            .iter()
            .map(|directory| places.portable(&text(directory.as_os_str())))
            .collect(),
        read_files,
        outputs,
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&plan_file)
        .map_err(|error| Error::new(format!("opening {}: {error}", plan_file.display())))?;
    file.write_all(entry.write().as_bytes())?;
    Ok(())
}

/// Answer `build` from `store`.
fn replay(store: &Path, build: &Build) -> Result<()> {
    let key = key(build, &Places::here())?;
    let stored = store.join(&key);
    if !stored.is_dir() {
        return Err(Error::new(format!(
            "{} was not built on Ferrix: {} has no build {key}",
            build.what,
            store.display()
        )));
    }
    for (index, output) in build.outputs.iter().enumerate() {
        copy_output(&stored.join(index.to_string()), output)?;
    }
    println!("  {}: Ferrix's build {}", build.what, short(&key));
    Ok(())
}

/// The first twelve digits of a key, for the console.
fn short(key: &str) -> &str {
    key.get(..12).unwrap_or(key)
}

/// Make every build of the plan in `directory` here, keeping each one's
/// outputs in `directory/store`, and say how many were made.
///
/// A build that fails does not stop the ones after it; the error at the end
/// names every one that failed, and a build whose input a failed one should
/// have made fails too.
///
/// # Errors
///
/// When the plan cannot be read, or any build failed.
pub(crate) fn execute(directory: &Path) -> Result<usize> {
    let plan_file = directory.join("plan");
    let plan = std::fs::read_to_string(&plan_file)
        .map_err(|error| Error::new(format!("reading {}: {error}", plan_file.display())))?;
    let builds = parse(&plan)?;
    let store = directory.join("store");
    std::fs::create_dir_all(&store)?;
    let places = Places::here();
    // Each recorded output's digest, and where its counterpart made here is.
    let mut made: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut failed = Vec::new();
    for (index, planned) in builds.iter().enumerate() {
        println!(
            "builds: {} of {}: {}",
            index + 1,
            builds.len(),
            planned.what
        );
        match execute_one(planned, directory, &store, &places, &made) {
            Ok(outputs) => made.extend(outputs),
            Err(error) => {
                println!("builds: {} failed: {error}", planned.what);
                failed.push(planned.what.clone());
            }
        }
    }
    if failed.is_empty() {
        println!("builds: all {} made", builds.len());
        Ok(builds.len())
    } else {
        Err(Error::new(format!(
            "{} of {} builds failed: {}",
            failed.len(),
            builds.len(),
            failed.join(", ")
        )))
    }
}

/// Make one planned build here and store its outputs; answer each recorded
/// digest with where the output made here was stored.
fn execute_one(
    planned: &Planned,
    directory: &Path,
    store: &Path,
    places: &Places,
    made: &BTreeMap<String, PathBuf>,
) -> Result<Vec<(String, PathBuf)>> {
    let mut build = Build::new(
        planned.program,
        planned.what.clone(),
        places.local(&planned.dir),
    );
    build.args = planned.args.iter().map(|arg| places.local(arg)).collect();
    build.env = planned
        .env
        .iter()
        .map(|(key, value)| (key.clone(), places.local(value)))
        .collect();
    for (name, source) in &planned.inputs {
        let file = match source {
            Source::Output(digest) => made.get(digest).cloned().ok_or_else(|| {
                Error::new(format!(
                    "its input {name} was to come from a build that failed"
                ))
            })?,
            Source::File(digest) => directory.join("files").join(digest),
        };
        build.inputs.push((name.clone(), file));
    }
    build.reads = planned
        .reads
        .iter()
        .map(|directory| PathBuf::from(places.local(directory)))
        .collect();
    for (file, digest) in &planned.read_files {
        let file = PathBuf::from(places.local(file));
        if !file.is_file() || digest_of(&file)? != *digest {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ =
                std::fs::copy(directory.join("files").join(digest), &file).map_err(|error| {
                    Error::new(format!("putting {} in place: {error}", file.display()))
                })?;
        }
    }
    build.outputs = planned
        .outputs
        .iter()
        .map(|(file, _)| PathBuf::from(places.local(file)))
        .collect();
    // Its key is known before it is made: what a build is and what it reads.
    // A store carried in from an earlier run that has it already answers it,
    // so that a plan whose run stopped at a failure picks up where it did.
    let key = key(&build, places)?;
    let kept = store.join(&key);
    let done = (0..build.outputs.len()).all(|index| kept.join(index.to_string()).exists());
    if done && !build.outputs.is_empty() {
        let mut stored = Vec::new();
        for (index, (output, (_, digest))) in build.outputs.iter().zip(&planned.outputs).enumerate()
        {
            let from = kept.join(index.to_string());
            copy_output(&from, output)?;
            stored.push((digest.clone(), from));
        }
        println!("builds: {} was made here already", planned.what);
        return Ok(stored);
    }
    build.make()?;
    std::fs::create_dir_all(&kept)?;
    let mut stored = Vec::new();
    for (index, (output, (_, digest))) in build.outputs.iter().zip(&planned.outputs).enumerate() {
        let to = kept.join(index.to_string());
        copy_output(output, &to)?;
        stored.push((digest.clone(), to));
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places() -> Places {
        Places {
            root: "/home/me/ferrix".to_owned(),
            target: "/home/me/ferrix/target".to_owned(),
            data: "/home/me/.local/share/ferrix".to_owned(),
        }
    }

    #[test]
    fn the_mode_is_read_from_the_variable() {
        assert_eq!(Mode::parse("").unwrap(), Mode::Build);
        assert_eq!(
            Mode::parse("record:/tmp/plan").unwrap(),
            Mode::Record("/tmp/plan".into())
        );
        assert_eq!(
            Mode::parse("replay:/tmp/plan/store").unwrap(),
            Mode::Replay("/tmp/plan/store".into())
        );
        assert_eq!(
            Mode::parse("plan:/tmp/plan").unwrap(),
            Mode::Plan("/tmp/plan".into())
        );
        assert!(Mode::parse("sideways:/tmp").is_err());
    }

    #[test]
    fn places_become_placeholders_target_first() {
        let places = places();
        assert_eq!(
            places.portable("/home/me/ferrix/target/zinc"),
            "${TARGET}/zinc"
        );
        assert_eq!(places.portable("/home/me/ferrix/zinc"), "${ROOT}/zinc");
        assert_eq!(
            places.portable("/home/me/.local/share/ferrix/busybox"),
            "${DATA}/busybox"
        );
        let there = Places {
            root: "/data/src".to_owned(),
            target: "/data/target".to_owned(),
            data: "/data/home/.local/share/ferrix".to_owned(),
        };
        assert_eq!(there.local("${TARGET}/zinc"), "/data/target/zinc");
        assert_eq!(there.local("${ROOT}/zinc"), "/data/src/zinc");
    }

    #[test]
    fn escaping_round_trips_spaces_newlines_and_percents() {
        for value in ["plain", "a b", "line\nline\n", "100%", "", "é ü"] {
            let escaped = escape(value);
            assert!(
                !escaped.contains(' ') && !escaped.contains('\n'),
                "{escaped}"
            );
            assert_eq!(unescape(&escaped).unwrap(), value);
        }
    }

    #[test]
    fn a_plan_reads_back_as_written() {
        let one = Planned {
            key: "k1".to_owned(),
            what: "cargo build (zinc)".to_owned(),
            program: Program::Cargo,
            dir: "${ROOT}/zinc".to_owned(),
            args: vec!["build".to_owned(), "--release".to_owned()],
            env: vec![("RUSTFLAGS".to_owned(), "-C a -C b".to_owned())],
            inputs: vec![],
            reads: vec![],
            read_files: vec![],
            outputs: vec![("${TARGET}/zinc/zinc".to_owned(), "d1".to_owned())],
        };
        let two = Planned {
            key: "k2".to_owned(),
            what: "cargo build -p ferrix-kernel".to_owned(),
            program: Program::Bash,
            dir: "${ROOT}".to_owned(),
            args: vec![],
            env: vec![(
                "FERRIX_INIT_SCRIPT".to_owned(),
                "echo hi\nexit 3\n".to_owned(),
            )],
            inputs: vec![
                ("FERRIX_INIT".to_owned(), Source::Output("d1".to_owned())),
                (
                    "FERRIX_INIT_COMMANDS".to_owned(),
                    Source::File("f9".to_owned()),
                ),
            ],
            reads: vec!["${DATA}/busybox/ferrousli/src".to_owned()],
            read_files: vec![(
                "${DATA}/busybox/ferrousli/src/busybox 1.37.0.tar.bz2".to_owned(),
                "f7".to_owned(),
            )],
            outputs: vec![("${TARGET}/k".to_owned(), "d2".to_owned())],
        };
        let text = one.write() + &two.write();
        assert_eq!(parse(&text).unwrap(), vec![one, two]);
        assert!(parse("build k\nwhat x\n").is_err());
        assert!(parse("what x\n").is_err());
    }

    #[test]
    fn a_key_follows_the_input_bytes_and_not_their_path() {
        let directory = std::env::temp_dir().join(format!("ferrix-builds-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let first = directory.join("a");
        let second = directory.join("b");
        std::fs::write(&first, b"same").unwrap();
        std::fs::write(&second, b"same").unwrap();
        let places = places();
        let build = |file: &Path| {
            Build::cargo("k", "/home/me/ferrix")
                .args(["build"])
                .env("CARGO_TARGET_DIR", "/home/me/ferrix/target/x")
                .input("FERRIX_INIT", file)
                .output("/home/me/ferrix/target/x/k")
        };
        let a = key(&build(&first), &places).unwrap();
        assert_eq!(a, key(&build(&second), &places).unwrap());
        std::fs::write(&second, b"different").unwrap();
        assert_ne!(a, key(&build(&second), &places).unwrap());
        // The same build on a machine whose tree is elsewhere.
        let there = Places {
            root: "/data/src".to_owned(),
            target: "/data/target".to_owned(),
            data: "/data/home/.local/share/ferrix".to_owned(),
        };
        let moved = Build::cargo("k", "/data/src")
            .args(["build"])
            .env("CARGO_TARGET_DIR", "/data/target/x")
            .input("FERRIX_INIT", &first)
            .output("/data/target/x/k");
        assert_eq!(a, key(&moved, &there).unwrap());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_output_is_kept_whole() {
        let root = std::env::temp_dir().join(format!("ferrix-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tree = root.join("tree");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/file"), b"bytes").unwrap();
        std::os::unix::fs::symlink("sub/file", tree.join("link")).unwrap();
        let before = digest_of_output(&tree).unwrap();
        let copy = root.join("copy");
        copy_output(&tree, &copy).unwrap();
        assert_eq!(digest_of_output(&copy).unwrap(), before);
        assert_eq!(
            std::fs::read_link(copy.join("link")).unwrap(),
            Path::new("sub/file")
        );
        // Copied over again, whatever was there goes.
        std::fs::write(copy.join("stale"), b"old").unwrap();
        copy_output(&tree, &copy).unwrap();
        assert!(!copy.join("stale").exists());
        std::fs::write(tree.join("sub/file"), b"other").unwrap();
        assert_ne!(digest_of_output(&tree).unwrap(), before);
        std::fs::remove_dir_all(&root).unwrap();
    }

    // `bash -c` with POSIX paths: the plans xtask records are carried out on
    // the Linux host that builds images, and on Windows `bash` may be WSL's,
    // which sees none of these paths.
    #[test]
    #[cfg(unix)]
    fn record_execute_and_replay_agree() {
        // A plan of two builds, the second reading the first's output, carried
        // out by `bash` in place of cargo; then each is replayed.
        let root = std::env::temp_dir().join(format!("ferrix-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let places = Places {
            root: text(root.as_os_str()),
            target: text(root.join("target").as_os_str()),
            data: text(root.join("data").as_os_str()),
        };
        let first_out = root.join("target/one");
        let second_out = root.join("target/two");
        let one = Planned {
            key: "one".to_owned(),
            what: "one".to_owned(),
            program: Program::Bash,
            dir: "${ROOT}".to_owned(),
            args: vec![
                "-c".to_owned(),
                "mkdir -p target && echo made > target/one".to_owned(),
            ],
            env: vec![],
            inputs: vec![],
            reads: vec![],
            read_files: vec![],
            outputs: vec![("${TARGET}/one".to_owned(), "host-one".to_owned())],
        };
        let two = Planned {
            key: "two".to_owned(),
            what: "two".to_owned(),
            program: Program::Bash,
            dir: "${ROOT}".to_owned(),
            args: vec![
                "-c".to_owned(),
                "cat \"$IN\" \"$IN\" > target/two".to_owned(),
            ],
            env: vec![],
            inputs: vec![("IN".to_owned(), Source::Output("host-one".to_owned()))],
            reads: vec![],
            read_files: vec![],
            outputs: vec![("${TARGET}/two".to_owned(), "host-two".to_owned())],
        };
        let store = root.join("store");
        std::fs::create_dir_all(&store).unwrap();
        let mut made = BTreeMap::new();
        made.extend(execute_one(&one, &root, &store, &places, &made).unwrap());
        let stored_one = made.get("host-one").unwrap().clone();
        made.extend(execute_one(&two, &root, &store, &places, &made).unwrap());
        assert_eq!(std::fs::read(&second_out).unwrap(), b"made\nmade\n");
        // Replayed, the second build names the stored first output as its
        // input, as xtask's own caller would after replaying the first.
        std::fs::remove_file(&second_out).unwrap();
        let replayed = Build::bash("two", &root)
            .args(["-c", "cat \"$IN\" \"$IN\" > target/two"])
            .input("IN", &stored_one)
            .output(&second_out);
        let key = key(&replayed, &places).unwrap();
        assert!(store.join(&key).is_dir());
        let _ = first_out;
        std::fs::remove_dir_all(&root).unwrap();
    }
}
