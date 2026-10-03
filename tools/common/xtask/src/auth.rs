//! Authentication for the image, and `test-auth` (`docs/AUTH.md` §7, P1.6).
//!
//! # Building it
//!
//! `src/user/system/linux/auth/` is a workspace of its own, built as init is: a static
//! program against the target's own musl, linked by rust-lld. [`carried`]
//! returns what an image carries for it: `/sbin/authd`, `/bin/passwd`,
//! `/bin/authctl`, the shipped service policies in
//! `/lib/ferrix/auth/services`, and `auth.socket` and `auth.service` in
//! `/lib/ferrix/units` with the socket wanted by `sockets.target`. It adds the
//! `auth` account (uid [`UID`]) that `authd` runs as. Only an image that
//! carries init can start it; the others carry none of it.
//!
//! # Seeds
//!
//! `--auth-seed ACCOUNT` asks on this terminal for a password,
//! `--auth-seed-file ACCOUNT=FILE` reads one from a file, and either is
//! hashed here with `src/lib/crypto/argon2` at the floor and carried as
//! `/lib/ferrix/auth/seed/ACCOUNT`. `authd` takes a seed as the account's
//! first password only while the account has none, so a password changed on
//! the machine survives the next image (`docs/AUTH.md` §5.3).
//!
//! # The test
//!
//! One boot per architecture of init with authd, typed at the console the
//! getty gives, as `test-init` is. The image seeds `ferrix` with a password
//! only this test knows, and carries a `gate` policy that lets root name any
//! account. Every refusal has a line of its own, and a sabotage that turns it
//! off (`--sabotage NAME`, which builds `authd` with that refusal gone) must
//! make exactly that line fail:
//!
//! | What is required | Its sabotage |
//! |---|---|
//! | the seeded password opens `ferrix`, and a wrong one fails after the delay | `accept-any` |
//! | an unknown account fails with a wrong password's words, and six failures answer as an account's six do | `tell-unknown` |
//! | uid 1000 naming root is refused before any prompt | `let-anyone-name` |
//! | the fifth attempt after four failures is refused unlooked-at | `no-throttle` |
//!
//! Beside those, with no control of their own because a sabotage of any of
//! them is a crash rather than a quiet wrong answer: `authd` runs as `auth`;
//! `passwd` changes the password and the change outlives a restart of
//! `authd` while the seed does not come back; a user cannot read the store;
//! and the audit log has the attempts and never the password.

use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::{self, SUCCESS_MARKER, Watching};
use crate::{Error, Result, cargo, fat, init, initramfs, native, zinc};

/// What only a sabotaged `authd` carries (`src/user/system/linux/auth/authd/src/sabotage.rs`).
const MARKER: &[u8] = b"FERRIX-AUTH-SABOTAGED-BUILD";

/// Set by `test-auth --sabotage` alone: the one run whose image may carry a
/// sabotaged `authd`.
static SABOTAGE_ASKED: AtomicBool = AtomicBool::new(false);

/// Refuse an image whose `authd` is sabotaged, unless `test-auth
/// --sabotage` is building it (ferrix-55's review, 2026-09-26).
///
/// # Errors
///
/// A sabotaged `authd` in any other image.
pub(crate) fn refuse_sabotaged(files: &[File]) -> Result<()> {
    if SABOTAGE_ASKED.load(Ordering::Relaxed) {
        return Ok(());
    }
    for file in files {
        if let Content::Bytes(bytes) = &file.content
            && file.path.ends_with("authd")
            && bytes.windows(MARKER.len()).any(|window| window == MARKER)
        {
            return Err(Error::new(format!(
                "{} is a sabotaged authd, for test-auth's negative controls only; no image may carry it",
                file.path
            )));
        }
    }
    Ok(())
}

/// The uid and gid of the `auth` account.
pub(crate) const UID: u32 = 90;

/// Its `/etc/passwd` line.
pub(crate) const PASSWD_LINE: &str = "auth:x:90:90:authd:/var/lib/ferrix/auth:/sbin/nologin\n";

/// Its `/etc/group` line, and `wheel` with `ferrix` in it: who may become
/// root with `su` and their own password (decision 5).
pub(crate) const GROUP_LINE: &str = "auth:x:90:\nwheel:x:10:ferrix\n";

/// The shipped policies and units, beside this module in the tree.
const SERVICES: &str = "src/user/system/linux/auth/services";
const UNITS: &str = "src/user/system/linux/auth/units";

/// The password the gate seeds `ferrix` with.
const GATE_PASSWORD: &str = "the gate own horse battery";

/// The password `passwd` gives `ferrix` in the gate.
const CHANGED: &str = "a second password, set on the guest";

/// How long to wait for one answer; a check under emulation is slow.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the getty prints once it holds the terminal.
const BANNER: &str = " on /dev/console";

/// What the kernel says as `reboot(2)` powers off.
const POWER_DOWN: &str = "reboot: Power down";

/// The gate's policy: root may name any account, and a failure waits two
/// seconds, as the shipped ones do.
const GATE_POLICY: &str = "[Service]\nDescription=test-auth's own service\nAccount=any\nMethods=password\nFailDelaySec=2\n";

/// The programs of `src/user/system/linux/auth/` for one architecture.
#[derive(Debug)]
pub(crate) struct Built {
    authd: PathBuf,
    passwd: PathBuf,
    authctl: PathBuf,
    login: PathBuf,
    su: PathBuf,
}

/// Build `src/user/system/linux/auth/` for `arch`, sabotaged as `sabotage` when one is
/// named, or `None` on an architecture it is not built for.
pub(crate) fn built(arch: Arch, sabotage: Option<&str>) -> Result<Option<Built>> {
    let Some(target) = zinc::target(arch) else {
        println!("  authd is not built for {} yet", arch.name());
        return Ok(None);
    };
    if sabotage.is_none() && std::env::var_os("FERRIX_AUTH_SABOTAGE").is_some() {
        return Err(Error::new(
            "FERRIX_AUTH_SABOTAGE is set in this environment, and would sabotage the authd of an \
             ordinary image; unset it (test-auth --sabotage NAME is how a control is asked for)",
        ));
    }
    // A sabotaged build has a directory of its own, so it can never be the
    // one an ordinary image picks up.
    let dir = match sabotage {
        Some(name) => format!("auth-sabotage-{name}"),
        None => "auth".to_owned(),
    };
    println!(
        "  building authd for {target}{}",
        sabotage.map_or(String::new(), |s| format!(", sabotaged: {s}"))
    );
    let target_dir = paths::target_dir().join(dir);
    let release = target_dir.join(target).join("release");
    let built = Built {
        authd: release.join("authd"),
        passwd: release.join("passwd"),
        authctl: release.join("authctl"),
        login: release.join("login"),
        su: release.join("su"),
    };
    let mut build = crate::builds::Build::cargo(
        format!("cargo build (auth) --target {target}"),
        paths::workspace_root().join("src/user/system/linux/auth"),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .env("RUSTFLAGS", zinc::RUSTFLAGS);
    if let Some(name) = sabotage {
        build = build.env("FERRIX_AUTH_SABOTAGE", name);
    }
    build
        .output(&built.authd)
        .output(&built.passwd)
        .output(&built.authctl)
        .output(&built.login)
        .output(&built.su)
        .run()?;
    Ok(Some(built))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// Every file in the tree directory `relative`, sorted, as (name, bytes).
fn shipped(relative: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let dir = paths::workspace_root().join(relative);
    let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|error| Error::new(format!("reading {}: {error}", dir.display())))?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?.to_owned();
            Some(read(&path).map(|bytes| (name, bytes)))
        })
        .collect()
}

/// What an image carries for authentication on `arch`: the programs, the
/// policies and the units. Empty on an architecture it is not built for.
/// The `auth` account's lines are the caller's to add to its
/// `/etc/passwd` and `/etc/group`: [`PASSWD_LINE`], [`GROUP_LINE`].
pub(crate) fn carried(arch: Arch, sabotage: Option<&str>) -> Result<Vec<File>> {
    let Some(built) = built(arch, sabotage)? else {
        return Ok(Vec::new());
    };
    let file = |path: String, mode: u32, bytes: Vec<u8>| File {
        path,
        mode,
        content: Content::Bytes(bytes),
    };
    let mut files = vec![
        file("sbin/authd".to_owned(), 0o755, read(&built.authd)?),
        file("bin/passwd".to_owned(), 0o755, read(&built.passwd)?),
        file("bin/authctl".to_owned(), 0o755, read(&built.authctl)?),
        // Ferrix's own, in busybox's place (`docs/AUTH.md` §6.2).
        file("bin/login".to_owned(), 0o755, read(&built.login)?),
        // Set-uid root (`docs/AUTH.md` §4.2): a member of wheel becomes root
        // with their own password. In busybox's place.
        file("bin/su".to_owned(), 0o4755, read(&built.su)?),
    ];
    for (name, bytes) in shipped(SERVICES)? {
        files.push(file(
            format!("lib/ferrix/auth/services/{name}"),
            0o644,
            bytes,
        ));
    }
    for (name, bytes) in shipped(UNITS)? {
        files.push(file(format!("lib/ferrix/units/{name}"), 0o644, bytes));
    }
    files.push(File {
        path: "lib/ferrix/units/sockets.target.wants/auth.socket".to_owned(),
        mode: 0o777,
        content: Content::Link("/lib/ferrix/units/auth.socket".to_owned()),
    });
    Ok(files)
}

/// What `cargo xtask run --auth-seed ACCOUNT` or `--login` adds to the image
/// init boots: authd with its units and policies, the seeds, an
/// `/etc/passwd` and `/etc/group` with root, `ferrix` and `auth`, which that
/// image has none of otherwise, and with `--login` the getty's drop-in.
/// Nothing when neither was asked for.
pub(crate) fn with_seeds(arch: Arch, args: &Args) -> Result<Vec<File>> {
    if args.auth_seeds.is_empty() && args.auth_seed_files.is_empty() && !args.login {
        return Ok(Vec::new());
    }
    let mut files = carried(arch, None)?;
    if files.is_empty() {
        return Err(Error::new(format!(
            "--auth-seed: authd is not built for {arch}"
        )));
    }
    files.extend(seeds(args)?);
    for (path, text) in [("etc/passwd", passwd()), ("etc/group", group())] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o644,
            content: Content::Bytes(text.into_bytes()),
        });
    }
    if args.login {
        // `ExecStart=` emptied, then getty with `--login`: the console asks
        // who is there instead of opening root's shell.
        files.push(File {
            path: "etc/ferrix/units/getty@.service.d/login.conf".to_owned(),
            mode: 0o644,
            content: Content::Bytes(LOGIN_DROP_IN.as_bytes().to_vec()),
        });
    }
    Ok(files)
}

/// `run --login`'s drop-in for every getty, and a session desktop's.
pub(crate) const LOGIN_DROP_IN: &str = "# Carried by `cargo xtask run --login` (docs/AUTH.md §6.2).\n\
                             [Service]\n\
                             ExecStart=\n\
                             ExecStart=/sbin/getty --login %i\n";

/// The seeds `--auth-seed` and `--auth-seed-file` ask for, as carried files.
pub(crate) fn seeds(args: &Args) -> Result<Vec<File>> {
    let mut files = Vec::new();
    for account in &args.auth_seeds {
        let password = ask_on_terminal(account)?;
        files.push(seed(account, &password)?);
    }
    for (account, from) in &args.auth_seed_files {
        let text = std::fs::read_to_string(from)
            .map_err(|error| Error::new(format!("reading {}: {error}", from.display())))?;
        let password = text.lines().next().unwrap_or_default();
        files.push(seed(account, password)?);
    }
    Ok(files)
}

/// Ask for `account`'s password on this terminal, with its echo off while
/// it is typed, twice.
fn ask_on_terminal(account: &str) -> Result<String> {
    let quiet = |on: bool| {
        let _ = std::process::Command::new("stty")
            .arg(if on { "-echo" } else { "echo" })
            .stdin(std::process::Stdio::inherit())
            .status();
    };
    let read_one = |what: &str| -> Result<String> {
        eprint!("{what}");
        let _ = std::io::stderr().flush();
        quiet(true);
        let mut line = String::new();
        let got = std::io::stdin().lock().read_line(&mut line);
        quiet(false);
        eprintln!();
        let _ = got.map_err(|error| Error::new(format!("reading the password: {error}")))?;
        Ok(line.trim_end_matches(['\n', '\r']).to_owned())
    };
    let first = read_one(&format!("First password for {account} on the image: "))?;
    let again = read_one("Again: ")?;
    if first != again {
        return Err(Error::new("the two passwords did not match"));
    }
    if first.is_empty() {
        return Err(Error::new("a password may not be empty"));
    }
    Ok(first)
}

/// A seed file for `account`: `password` hashed here at the floor.
pub(crate) fn seed(account: &str, password: &str) -> Result<File> {
    if account.is_empty()
        || !account
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        return Err(Error::new(format!("{account:?} is not an account name")));
    }
    let params = ferrix_argon2::Params {
        memory_kib: 19_456,
        passes: 2,
        lanes: 1,
    };
    let mut salt = [0_u8; 16];
    let mut urandom = std::fs::File::open("/dev/urandom")
        .map_err(|error| Error::new(format!("opening /dev/urandom: {error}")))?;
    std::io::Read::read_exact(&mut urandom, &mut salt)
        .map_err(|error| Error::new(format!("reading /dev/urandom: {error}")))?;
    let blocks = params
        .blocks()
        .map_err(|why| Error::new(format!("{why:?}")))?;
    let mut memory = vec![ferrix_argon2::Block::ZERO; blocks];
    let mut tag = [0_u8; 32];
    ferrix_argon2::hash(
        &params,
        &ferrix_argon2::Inputs {
            password: password.as_bytes(),
            salt: &salt,
            secret: &[],
            associated: &[],
        },
        &mut memory,
        &mut tag,
    )
    .map_err(|why| Error::new(format!("hashing the seed: {why:?}")))?;
    let encoded = ferrix_argon2::phc::Encoded::new(params, &salt, &tag)
        .map_err(|why| Error::new(format!("the seed: {why:?}")))?;
    Ok(File {
        path: format!("lib/ferrix/auth/seed/{account}"),
        mode: 0o600,
        content: Content::Bytes(format!("{encoded}\n").into_bytes()),
    })
}

/// The test's accounts: root, `ferrix`, and `auth`.
fn passwd() -> String {
    format!("root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/:/bin/sh\n{PASSWD_LINE}")
}

fn group() -> String {
    format!("root:x:0:\nferrix:x:1000:\n{GROUP_LINE}")
}

/// `cargo xtask test-auth`.
///
/// # Errors
///
/// When an image cannot be built, a boot fails, or a line the module
/// documentation lists is missing, with the serial log's path.
pub(crate) fn test_auth(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        test_arch(arch, args)?;
    }
    Ok(())
}

fn test_arch(arch: Arch, args: &Args) -> Result<()> {
    SABOTAGE_ASKED.store(args.sabotage.is_some(), Ordering::Relaxed);
    let shell = zinc::built(arch)?
        .ok_or_else(|| Error::new(format!("zinc could not be built for {arch}")))?;
    let shell = read(&shell)?;
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let auth = carried(arch, args.sabotage.as_deref())?;
    if auth.is_empty() {
        return Err(Error::new(format!("authd is not built for {arch}")));
    }
    files.extend(auth);
    files.push(seed("ferrix", GATE_PASSWORD)?);
    files.push(File {
        path: "etc/ferrix/auth/services/gate".to_owned(),
        mode: 0o644,
        content: Content::Bytes(GATE_POLICY.as_bytes().to_vec()),
    });
    for path in ["bin/sh", "bin/zinc"] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o755,
            content: Content::Bytes(shell.clone()),
        });
    }
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(read(&crate::busybox::program(arch)?)?),
    });
    for applet in ["su", "cat", "grep", "date", "printf"] {
        files.push(File {
            path: format!("bin/{applet}"),
            mode: 0o777,
            content: Content::Link("busybox".to_owned()),
        });
    }
    for (path, text) in [("etc/passwd", passwd()), ("etc/group", group())] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o644,
            content: Content::Bytes(text.into_bytes()),
        });
    }
    println!("  {arch}: building an image whose init starts authd");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let archive = initramfs::build(None, &natives, None, &files)?;
    let options = format!("{}\n", qemu::init_option(init::PATH));
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;
    println!(
        "  {arch}: typing at the console the getty gives (timeout {}s)",
        args.timeout
    );
    let mut failures: Vec<String> = Vec::new();
    let mut timings: Vec<String> = Vec::new();
    let _lines = qemu::watch_then(arch, &image, &kernel, args, SUCCESS_MARKER, |at| {
        session(at, &mut failures, &mut timings)
    })?;
    for line in &timings {
        println!("  {arch}: {line}");
    }
    if !failures.is_empty() {
        let mut message = format!("{arch}: authd did not do what docs/AUTH.md requires:\n");
        for failure in &failures {
            message.push_str(&format!("    - {failure}\n"));
        }
        message.push_str(&format!(
            "  Serial output is in {}",
            paths::build_dir(arch).join("serial.log").display()
        ));
        return Err(Error::new(message));
    }
    println!(
        "  {arch}: authd ran as auth, opened ferrix for its password alone, failed a wrong one \
         and an unknown account alike after the delay, refused a user naming root, throttled \
         the fifth attempt, kept a changed password across a restart, kept the store from a \
         user, and logged every attempt and no password"
    );
    Ok(())
}

/// Every line seen so far.
fn everything(at: &Watching<'_>) -> Vec<String> {
    at.lines().iter().chain(at.after()).cloned().collect()
}

/// Type `command` with a marker after it, and give every `authctl: ` and
/// `passwd: ` line it printed, in order.
fn run(at: &mut Watching<'_>, command: &str, marker: &str) -> Result<Vec<String>> {
    let before = at.after().len();
    let keys = format!("{command}; m={marker}; echo \"$m-done\"\n");
    at.type_in(keys.as_bytes())?;
    let done = format!("{marker}-done");
    let deadline = Instant::now() + PATIENCE * 2;
    let _ = at.read_more(deadline, |lines| {
        lines
            .get(before..)
            .unwrap_or_default()
            .iter()
            .any(|line| line.trim() == done)
    })?;
    Ok(at
        .after()
        .get(before..)
        .unwrap_or_default()
        .iter()
        .map(|line| line.trim().to_owned())
        .filter(|line| {
            line.starts_with("authctl: ")
                || line.starts_with("passwd: ")
                || line.starts_with(marker)
        })
        .collect())
}

/// The one answer line of `lines` that starts with `prefix`.
fn answer<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    lines
        .iter()
        .map(String::as_str)
        .find(|line| line.starts_with(prefix))
}

const FAILED: &str = "authctl: failed: Authentication failed (retry after 0 ms)";
const ACCEPTED: &str = "authctl: accepted ferrix (1000)";

fn session(
    at: &mut Watching<'_>,
    failures: &mut Vec<String>,
    timings: &mut Vec<String>,
) -> Result<()> {
    let deadline = Instant::now() + PATIENCE * 2;
    let up = at.read_more(deadline, |lines| {
        lines.iter().any(|line| line.contains(BANNER))
    })?;
    if !up {
        failures.push("the getty never printed its banner".into());
        return Ok(());
    }
    if !at.wait_for_shell(Instant::now() + PATIENCE)? {
        failures.push("the getty's shell never answered at the console".into());
        return Ok(());
    }
    seeded(at, failures, timings)?;
    wrong_and_unknown(at, failures)?;
    alike(at, failures)?;
    naming(at, failures)?;
    throttle(at, failures)?;
    change(at, failures)?;
    store_and_log(at, failures)?;
    // What the hash cost here, as authd timed it when passwd set a password.
    for line in everything(at) {
        if let Some(at_floor) = line.split("authd: argon2id at the floor").nth(1) {
            timings.push(format!("argon2id at the floor{}", at_floor.trim_end()));
        }
    }
    at.type_in(b"svc poweroff\n")?;
    let deadline = Instant::now() + PATIENCE * 4;
    let _ = at.read_more(deadline, |lines| {
        lines.iter().any(|line| line.contains(POWER_DOWN))
    })?;
    Ok(())
}

/// The seed was imported, and authd runs as `auth`.
fn seeded(
    at: &mut Watching<'_>,
    failures: &mut Vec<String>,
    timings: &mut Vec<String>,
) -> Result<()> {
    // The seed was imported, and authd became auth to answer.
    let status = run(at, "authctl status ferrix", "st")?;
    if answer(&status, "authctl: ferrix:")
        != Some("authctl: ferrix: a password is set; not throttled")
    {
        failures.push(format!(
            "authctl status ferrix did not find the seeded password: {status:?}"
        ));
    }
    let all = everything(at);
    let running = format!("authd: running as auth (uid {UID})");
    if !all.iter().any(|line| line.contains(&running)) {
        failures.push(format!("authd did not say `{running}`"));
    }
    if all
        .iter()
        .any(|line| line.contains("authd: FERRIX-AUTH-SABOTAGED-BUILD"))
    {
        timings.push("this run's authd is sabotaged, as --sabotage asked".into());
    }
    Ok(())
}

/// The seeded password opens ferrix, a wrong one fails after the delay, and an unknown account fails alike.
fn wrong_and_unknown(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // The seeded password opens ferrix; a wrong one fails after the delay.
    let right = run(
        at,
        &format!("echo '{GATE_PASSWORD}' | authctl try gate ferrix"),
        "ok",
    )?;
    if answer(&right, "authctl: ") != Some(ACCEPTED) {
        failures.push(format!(
            "the seeded password did not open ferrix: {right:?}"
        ));
    }
    let wrong = run(
        at,
        "t0=$(date +%s); echo 'not it' | authctl try gate ferrix; t1=$(date +%s); echo \"wr-took $((t1-t0))\"",
        "wr",
    )?;
    if answer(&wrong, "authctl: ") != Some(FAILED) {
        failures.push(format!(
            "a wrong password for ferrix did not fail as a wrong password: {wrong:?} (accept-any)"
        ));
    }
    let took = answer(&wrong, "wr-took ")
        .and_then(|line| line.strip_prefix("wr-took "))
        .and_then(|s| s.parse::<u64>().ok());
    if took.is_none_or(|seconds| seconds < 2) {
        failures.push(format!(
            "a wrong password's FAILED came before the two seconds' delay: {took:?}"
        ));
    }

    // An unknown account fails in a wrong password's words.
    let unknown = run(at, "echo 'not it' | authctl try gate nobody", "un")?;
    if answer(&unknown, "authctl: ") != Some(FAILED) {
        failures.push(format!(
            "an unknown account did not fail as a wrong password does: {unknown:?} (tell-unknown)"
        ));
    }
    Ok(())
}

/// Six failures in a row answer the same for an account and for a name that is none.
fn alike(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // Six failures in a row answer the same for an account and for a name
    // that is none, throttle and all.
    let _ = run(at, "authctl reset ferrix", "r0")?;
    let six = |name: &str| {
        format!("for i in 1 2 3 4 5 6; do echo 'not it' | authctl try gate {name}; done")
    };
    let real = run(at, &six("ferrix"), "s1")?;
    let ghost = run(at, &six("ghost"), "s2")?;
    let words = |lines: &[String]| -> Vec<String> {
        lines
            .iter()
            .filter(|line| line.starts_with("authctl: "))
            .map(|line| {
                // The words, not the numbers: how many seconds a throttle has
                // left is a matter of when the line was typed.
                let words = line.split(" (retry after").next().unwrap_or_default();
                if words.starts_with("authctl: failed: wait ") {
                    "authctl: failed: wait".to_owned()
                } else {
                    words.to_owned()
                }
            })
            .collect()
    };
    if words(&real).len() != 6 || words(&real) != words(&ghost) {
        failures.push(format!(
            "six failures answered differently for ferrix and for ghost, which is no account: {:?} against {:?} (tell-unknown)",
            words(&real),
            words(&ghost)
        ));
    }
    let _ = run(at, "authctl reset ferrix", "r1")?;
    Ok(())
}

/// A user may use its own account and may not name another.
fn naming(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // uid 1000 may use its own account, and may not name root.
    let own = run(
        at,
        &format!("su ferrix -c \"echo '{GATE_PASSWORD}' | authctl try hyprlock\""),
        "own",
    )?;
    if answer(&own, "authctl: ") != Some(ACCEPTED) {
        failures.push(format!(
            "ferrix could not unlock with its own password through hyprlock's policy: {own:?}"
        ));
    }
    let named = run(at, "su ferrix -c \"echo x | authctl try gate root\"", "nm")?;
    if answer(&named, "authctl: ")
        != Some("authctl: unavailable: only root may name another account")
    {
        failures.push(format!(
            "uid 1000 naming root was not refused before any prompt: {named:?} (let-anyone-name)"
        ));
    }
    Ok(())
}

/// Four failures in a row, then the right password, which is refused unlooked-at.
fn throttle(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // Four failures in a row, then the right password: refused unlooked-at.
    let throttled = run(
        at,
        &format!(
            "for i in 1 2 3 4; do echo 'not it' | authctl try gate ferrix; done; echo '{GATE_PASSWORD}' | authctl try gate ferrix"
        ),
        "th",
    )?;
    let last = throttled.iter().rfind(|line| line.starts_with("authctl: "));
    if !last.is_some_and(|line| line.starts_with("authctl: failed: wait ")) {
        failures.push(format!(
            "the attempt after four failures was not throttled: {throttled:?} (no-throttle)"
        ));
    }
    let reset = run(at, "authctl reset ferrix", "rs")?;
    if answer(&reset, "authctl: ferrix:")
        != Some("authctl: ferrix: a password is set; not throttled")
    {
        failures.push(format!("root could not reset ferrix's throttle: {reset:?}"));
    }
    Ok(())
}

/// passwd changes a password, which outlives a restart of authd.
fn change(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // passwd, as root, sets a new password; it outlives a restart of authd,
    // and the seed does not come back.
    let changed = run(
        at,
        &format!("printf '%s\\n%s\\n' '{CHANGED}' '{CHANGED}' | passwd ferrix"),
        "pw",
    )?;
    if answer(&changed, "passwd: ") != Some("passwd: the password for ferrix is changed") {
        failures.push(format!(
            "passwd ferrix did not change the password: {changed:?}"
        ));
    }
    let _ = run(at, "svc restart auth.service", "re")?;
    let after_new = run(
        at,
        &format!("echo '{CHANGED}' | authctl try gate ferrix"),
        "an",
    )?;
    if answer(&after_new, "authctl: ") != Some(ACCEPTED) {
        failures.push(format!(
            "the changed password did not open ferrix after authd restarted: {after_new:?}"
        ));
    }
    let after_old = run(
        at,
        &format!("echo '{GATE_PASSWORD}' | authctl try gate ferrix"),
        "ao",
    )?;
    if answer(&after_old, "authctl: ") != Some(FAILED) {
        failures.push(format!(
            "the seed's password still opened ferrix after it was changed: {after_old:?}"
        ));
    }
    Ok(())
}

/// The store is no user's to read, and the log holds attempts and no password.
fn store_and_log(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    // The store is not a user's to read; the log has attempts and no password.
    let denied = run(
        at,
        "su ferrix -c 'cat /var/lib/ferrix/auth/users/ferrix' >/dev/null 2>&1 || echo \"sd-denied\"",
        "sd",
    )?;
    if answer(&denied, "sd-denied").is_none() {
        failures.push("uid 1000 could read ferrix's credential record".into());
    }
    let logged = run(
        at,
        &format!(
            "echo \"lg-failed $(grep -c result=failed /var/log/ferrix/auth.log) lg-secret $(grep -c -e '{GATE_PASSWORD}' -e '{CHANGED}' /var/log/ferrix/auth.log)\""
        ),
        "lg",
    )?;
    match answer(&logged, "lg-failed ") {
        Some(line) if line.ends_with(" lg-secret 0") && !line.starts_with("lg-failed 0 ") => {}
        other => failures.push(format!(
            "the audit log is not the attempts without the passwords: {other:?}"
        )),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CHANGED, GATE_PASSWORD, GATE_POLICY};

    /// The session types each password inside single quotes, so one with a
    /// quote in it would leave the shell waiting for the rest of a string
    /// (it did, on the first boot).
    #[test]
    fn the_gate_passwords_quote_in_a_shell() {
        for password in [GATE_PASSWORD, CHANGED] {
            assert!(
                !password.contains('\'') && !password.contains('\\'),
                "{password:?}"
            );
        }
        assert!(GATE_POLICY.starts_with("[Service]\n"));
    }
}
