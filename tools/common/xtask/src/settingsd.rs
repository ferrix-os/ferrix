//! The settings writer for the image, and `test-settings`
//! (`docs/SETTINGS-WRITER.md` §9).
//!
//! # Building it
//!
//! `src/user/system/linux/settingsd/` is a workspace of its own, built as
//! authd's is: static programs against the target's own musl, linked by
//! rust-lld. [`carried`] returns what an image carries for it:
//! `/sbin/settingsd`, `/bin/settingsctl`, and `settings.socket` and
//! `settings.service` in `/lib/ferrix/units` with the socket wanted by
//! `sockets.target`. Its `authd` policies (`settings.*`) are authd's, in
//! `src/user/system/linux/auth/services`, and come with authd. Only an image
//! that carries init and authd can use it: the desktop's.
//!
//! # The test
//!
//! One boot of init with authd and settingsd, typed at the console the
//! getty gives, as `test-auth` is. Three accounts: root, `ferrix` (uid
//! 1000, in wheel) and `guest` (uid 1001, not), both with seeded passwords.
//! Each line below has a sentence of its own when it fails, and the
//! certification consultant's controls (ledger line 3482, condition 4) are
//! `gate.sh control` edits that must make exactly that sentence appear:
//!
//! | What is required | Control |
//! |---|---|
//! | guest, with its own right password, is refused both requests, and nothing changes | N1: settingsd treats every peer as root |
//! | ferrix with a wrong password is refused, and nothing changes | N2: settingsd takes a FAILED as ACCEPTED |
//! | ferrix with its password renames the machine (`/etc/hostname` and `hostname`) and enables `sshd.service` (its link) | |
//! | a unit not on the list, sent as given, is refused by settingsd itself | N3: the unit list is skipped |
//! | a dry run as guest says what would change and changes nothing | |
//! | `Methods=none` in a drop-in for `settings.hostname` lets ferrix in with no prompt, and still refuses guest | |
//! | `Methods=none` in a drop-in for a root-only wheel service that is not a settings one still asks for the password | N4: authd's bound is removed |
//! | the audit lines are in `svc log settings.service`, and hold no password | |

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::qemu::{self, SUCCESS_MARKER, Watching};
use crate::task::Task;
use crate::{Error, Result, cargo, fat, init, initramfs, native, zinc};

/// This module's `cargo xtask` commands (`crate::task`).
pub(crate) const TASKS: &[Task] = &[Task {
    name: "test-settings",
    help_at: 465,
    help: &[
        "test-settings Boot init with authd and settingsd and require docs/SETTINGS-WRITER.md's rules: a user",
        "              outside wheel refused, a wrong password refused, an administrator's change made",
    ],
    run: |_, args| test_settings(args),
}];

/// The workspace.
const DIR: &str = "src/user/system/linux/settingsd";

/// The units, beside the programs in the tree.
const UNITS: &str = "src/user/system/linux/settingsd/units";

/// The passwords the gate seeds.
const FERRIX_PASSWORD: &str = "ferrix own settings horse";
const GUEST_PASSWORD: &str = "guest own settings battery";
const WRONG_PASSWORD: &str = "not the right one";

/// How long to wait for one answer; a hash under emulation is slow.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the getty prints once it holds the terminal.
const BANNER: &str = " on /dev/console";

/// What the kernel says as `reboot(2)` powers off.
const POWER_DOWN: &str = "reboot: Power down";

/// A stand-in `sshd.service` with an `[Install]` section: what the Settings
/// page switches. Nothing starts it in the test; only its link is looked at.
const SSHD_UNIT: &str = "[Unit]\nDescription=test-settings' stand-in for sshd\n\n\
                         [Service]\nExecStart=/bin/sleep 100000\n\n\
                         [Install]\nWantedBy=multi-user.target\n";

/// A root-only wheel service of the qualifying shape that is not the
/// settings writer's: `Methods=none` must not loosen it (N4).
const MECHANISM_POLICY: &str = "[Service]\nDescription=test-settings' other mechanism\n\
                                Account=any\nCallers=root\nTargetGroup=wheel\nMethods=password\n";

/// The drop-in that asks for no password.
const NO_PASSWORD: &str = "[Service]\nMethods=none\n";

/// The programs of `src/user/system/linux/settingsd/` for one architecture.
#[derive(Debug)]
pub(crate) struct Built {
    settingsd: PathBuf,
    settingsctl: PathBuf,
}

/// Build the workspace for `arch`, or `None` where there is no musl target.
///
/// # Errors
///
/// A build that fails.
pub(crate) fn built(arch: Arch) -> Result<Option<Built>> {
    let Some(target) = zinc::target(arch) else {
        println!("  settingsd is not built for {} yet", arch.name());
        return Ok(None);
    };
    println!("  building settingsd for {target}");
    let target_dir = paths::target_dir().join("settingsd");
    let release = target_dir.join(target).join("release");
    let built = Built {
        settingsd: release.join("settingsd"),
        settingsctl: release.join("settingsctl"),
    };
    crate::builds::Build::cargo(
        format!("cargo build (settingsd) --target {target}"),
        paths::workspace_root().join(DIR),
    )
    .args(["build", "--release", "--target", target])
    .env("CARGO_TARGET_DIR", &target_dir)
    .env("RUSTFLAGS", zinc::RUSTFLAGS)
    .env(&crate::apps::linker_variable(target), "rust-lld")
    .output(&built.settingsd)
    .output(&built.settingsctl)
    .run()?;
    Ok(Some(built))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// What an image carries for the settings writer on `arch`: the programs
/// and the units. Empty on an architecture it is not built for.
///
/// # Errors
///
/// A build that fails, or a unit that cannot be read.
pub(crate) fn carried(arch: Arch) -> Result<Vec<File>> {
    let Some(built) = built(arch)? else {
        return Ok(Vec::new());
    };
    let file = |path: &str, mode: u32, bytes: Vec<u8>| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(bytes),
    };
    let mut files = vec![
        file("sbin/settingsd", 0o755, read(&built.settingsd)?),
        file("bin/settingsctl", 0o755, read(&built.settingsctl)?),
    ];
    for name in ["settings.socket", "settings.service"] {
        let bytes = read(&paths::workspace_root().join(UNITS).join(name))?;
        files.push(file(&format!("lib/ferrix/units/{name}"), 0o644, bytes));
    }
    files.push(File {
        path: "lib/ferrix/units/sockets.target.wants/settings.socket".to_owned(),
        mode: 0o777,
        content: Content::Link("/lib/ferrix/units/settings.socket".to_owned()),
    });
    Ok(files)
}

/// `cargo xtask test-settings`.
///
/// # Errors
///
/// When an image cannot be built, a boot fails, or a line the module
/// documentation lists is missing, with the serial log's path.
pub(crate) fn test_settings(args: &Args) -> Result<()> {
    for arch in args.arches()? {
        test_arch(arch, args)?;
    }
    Ok(())
}

/// The image's files beside init's, authd's and settingsd's own.
fn test_files(shell: &[u8], busybox: Vec<u8>) -> Result<Vec<File>> {
    let bytes = |path: &str, mode: u32, text: &str| File {
        path: path.to_owned(),
        mode,
        content: Content::Bytes(text.as_bytes().to_vec()),
    };
    let mut files = vec![
        crate::auth::seed("ferrix", FERRIX_PASSWORD)?,
        crate::auth::seed("guest", GUEST_PASSWORD)?,
        bytes("lib/ferrix/units/sshd.service", 0o644, SSHD_UNIT),
        bytes(
            "etc/ferrix/auth/services/gate-mechanism",
            0o644,
            MECHANISM_POLICY,
        ),
        bytes(
            "etc/ferrix/auth/services/gate-mechanism.d/10-none.conf",
            0o644,
            NO_PASSWORD,
        ),
        bytes(
            "etc/passwd",
            0o644,
            &format!(
                "root:x:0:0:root:/:/bin/sh\nferrix:x:1000:1000:ferrix:/:/bin/sh\n\
                 guest:x:1001:1001:guest:/:/bin/sh\n{}",
                crate::auth::PASSWD_LINE
            ),
        ),
        bytes(
            "etc/group",
            0o644,
            &format!(
                "root:x:0:\nferrix:x:1000:\nguest:x:1001:\n{}",
                crate::auth::GROUP_LINE
            ),
        ),
        bytes("etc/hostname", 0o644, "ferrix\n"),
    ];
    for path in ["bin/sh", "bin/zinc"] {
        files.push(File {
            path: path.to_owned(),
            mode: 0o755,
            content: Content::Bytes(shell.to_vec()),
        });
    }
    files.push(File {
        path: "bin/busybox".to_owned(),
        mode: 0o755,
        content: Content::Bytes(busybox),
    });
    // busybox's `su` in place of Ferrix's: root becomes a user without a
    // password, so each request is asked as that user.
    for applet in [
        "su", "cat", "grep", "hostname", "ls", "mkdir", "printf", "sleep",
    ] {
        files.push(File {
            path: format!("bin/{applet}"),
            mode: 0o777,
            content: Content::Link("busybox".to_owned()),
        });
    }
    Ok(files)
}

fn test_arch(arch: Arch, args: &Args) -> Result<()> {
    let shell = zinc::built(arch)?
        .ok_or_else(|| Error::new(format!("zinc could not be built for {arch}")))?;
    let shell = read(&shell)?;
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    let auth = crate::auth::carried(arch, None)?;
    if auth.is_empty() {
        return Err(Error::new(format!("authd is not built for {arch}")));
    }
    files.extend(auth);
    let settings = carried(arch)?;
    if settings.is_empty() {
        return Err(Error::new(format!("settingsd is not built for {arch}")));
    }
    files.extend(settings);
    files.extend(test_files(&shell, read(&crate::busybox::program(arch)?)?)?);
    println!("  {arch}: building an image whose init starts authd and settingsd");
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
    let _lines = qemu::watch_then(arch, &image, &kernel, args, SUCCESS_MARKER, |at| {
        session(at, &mut failures)
    })?;
    if !failures.is_empty() {
        let mut message =
            format!("{arch}: settingsd did not do what docs/SETTINGS-WRITER.md requires:\n");
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
        "  {arch}: settingsd refused guest outside wheel and a wrong password, renamed the \
         machine and enabled sshd.service for ferrix, refused a unit not on its list, changed \
         nothing on a dry run, took Methods=none for settings.hostname alone, and logged \
         every request and no password"
    );
    Ok(())
}

/// Type `command` with `m` set to `marker` before it and a line naming the
/// marker after it, and give every line it printed that holds
/// `settingsctl: `, `authctl: ` or the marker, from there on. A command
/// says `$m-...` for a line of its own, so that the echo of what was typed,
/// which holds `$m` and not the marker's value, is never taken for it.
fn run(at: &mut Watching<'_>, command: &str, marker: &str) -> Result<Vec<String>> {
    let before = at.after().len();
    let keys = format!("m={marker}; {command}; echo \"$m-done\"\n");
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
        .filter_map(|line| {
            ["settingsctl: ", "authctl: ", marker]
                .iter()
                .filter_map(|word| line.find(word))
                .min()
                .and_then(|at| line.get(at..))
                .map(|rest| rest.trim().to_owned())
        })
        .collect())
}

/// The first line of `lines` that starts with `prefix`.
fn answer<'a>(lines: &'a [String], prefix: &str) -> Option<&'a str> {
    lines
        .iter()
        .map(String::as_str)
        .find(|line| line.starts_with(prefix))
}

/// `/etc/hostname` and the running name, as `NAME NAME`.
fn names(at: &mut Watching<'_>, marker: &str) -> Result<Option<String>> {
    let lines = run(
        at,
        "echo \"$m-names $(cat /etc/hostname) $(hostname)\"",
        marker,
    )?;
    let prefix = format!("{marker}-names ");
    Ok(lines
        .iter()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(str::to_owned))
}

/// Whether the link at `path` exists.
fn linked(at: &mut Watching<'_>, path: &str, marker: &str) -> Result<bool> {
    let lines = run(
        at,
        &format!("ls {path} >/dev/null 2>&1 && echo \"$m-yes\""),
        marker,
    )?;
    Ok(answer(&lines, &format!("{marker}-yes")).is_some())
}

/// `settingsctl ARGS` as `user`, its password `password` (or none) on
/// standard input.
fn as_user(user: &str, password: Option<&str>, arguments: &str) -> String {
    match password {
        Some(password) => {
            format!("su {user} -c \"echo '{password}' | settingsctl {arguments}\"")
        }
        None => format!("su {user} -c \"settingsctl {arguments} </dev/null\""),
    }
}

const SSHD_LINK: &str = "/etc/ferrix/units/multi-user.target.wants/sshd.service";
const AUTH_LINK: &str = "/etc/ferrix/units/sockets.target.wants/auth.socket";

fn session(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
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
    outsider(at, failures)?;
    wrong_password(at, failures)?;
    administrator(at, failures)?;
    off_the_list(at, failures)?;
    dry_run(at, failures)?;
    no_password(at, failures)?;
    audit(at, failures)?;
    at.type_in(b"svc poweroff\n")?;
    let deadline = Instant::now() + PATIENCE * 4;
    let _ = at.read_more(deadline, |lines| {
        lines.iter().any(|line| line.contains(POWER_DOWN))
    })?;
    Ok(())
}

/// guest, outside wheel, with its own right password: refused both
/// requests, before any prompt, and nothing changes (N1).
fn outsider(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let named = run(
        at,
        &as_user("guest", Some(GUEST_PASSWORD), "set-hostname intruder"),
        "g1",
    )?;
    let switched = run(
        at,
        &as_user(
            "guest",
            Some(GUEST_PASSWORD),
            "service-enable-persistent sshd.service on",
        ),
        "g2",
    )?;
    let refused = "settingsctl: refused: not-permitted: guest is not in wheel";
    let names = names(at, "g3")?;
    let link = linked(at, SSHD_LINK, "g4")?;
    if answer(&named, "settingsctl: ") != Some(refused)
        || answer(&switched, "settingsctl: ") != Some(refused)
        || names.as_deref() != Some("ferrix ferrix")
        || link
    {
        failures.push(format!(
            "guest, outside wheel, was not refused with its own password: {named:?} {switched:?} \
             names {names:?} sshd link {link}"
        ));
    }
    Ok(())
}

/// ferrix with a wrong password: refused, nothing changes (N2).
fn wrong_password(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let wrong = run(
        at,
        &as_user("ferrix", Some(WRONG_PASSWORD), "set-hostname wrong"),
        "w1",
    )?;
    let names = names(at, "w2")?;
    let refused = answer(&wrong, "settingsctl: ")
        .is_some_and(|line| line.starts_with("settingsctl: refused: authentication-failed: "));
    if !refused || names.as_deref() != Some("ferrix ferrix") {
        failures.push(format!(
            "ferrix's wrong password was not refused: {wrong:?} names {names:?}"
        ));
    }
    Ok(())
}

/// ferrix with its password: the machine is renamed and sshd.service is
/// enabled.
fn administrator(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let named = run(
        at,
        &as_user("ferrix", Some(FERRIX_PASSWORD), "set-hostname box"),
        "a1",
    )?;
    let names = names(at, "a2")?;
    if answer(&named, "settingsctl: ")
        != Some("settingsctl: done: the computer is called box (was ferrix)")
        || names.as_deref() != Some("box box")
    {
        failures.push(format!(
            "ferrix with its password did not rename the machine: {named:?} names {names:?}"
        ));
    }
    let switched = run(
        at,
        &as_user(
            "ferrix",
            Some(FERRIX_PASSWORD),
            "service-enable-persistent sshd.service on",
        ),
        "a3",
    )?;
    let link = linked(at, SSHD_LINK, "a4")?;
    if answer(&switched, "settingsctl: ")
        != Some("settingsctl: done: sshd.service is on for every boot")
        || !link
    {
        failures.push(format!(
            "ferrix with its password did not enable sshd.service: {switched:?} link {link}"
        ));
    }
    Ok(())
}

/// A unit not on the list, sent as given: settingsd refuses it itself (N3).
fn off_the_list(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let other = run(
        at,
        &as_user(
            "ferrix",
            Some(FERRIX_PASSWORD),
            "--as-given service-enable-persistent auth.socket on",
        ),
        "l1",
    )?;
    let link = linked(at, AUTH_LINK, "l2")?;
    let refused = answer(&other, "settingsctl: ")
        .is_some_and(|line| line.starts_with("settingsctl: refused: invalid: "));
    if !refused || link {
        failures.push(format!(
            "settingsd did not refuse a unit off its list: {other:?} link {link}"
        ));
    }
    Ok(())
}

/// A dry run as guest says what would change, and changes nothing.
fn dry_run(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let would = run(
        at,
        &as_user("guest", None, "--dry-run set-hostname other"),
        "d1",
    )?;
    let names = names(at, "d2")?;
    if answer(&would, "settingsctl: ")
        != Some("settingsctl: would: call the computer other; it is called box")
        || names.as_deref() != Some("box box")
    {
        failures.push(format!(
            "a dry run did not say what it would do, or changed something: {would:?} names {names:?}"
        ));
    }
    Ok(())
}

/// `Methods=none`: for settings.hostname, ferrix with no password, guest
/// still refused; for another root-only wheel service, a password is still
/// asked (N4).
fn no_password(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let _ = run(
        at,
        &format!(
            "mkdir -p /etc/ferrix/auth/services/settings.hostname.d && printf '{}' > \
             /etc/ferrix/auth/services/settings.hostname.d/10-none.conf",
            NO_PASSWORD.replace('\n', "\\n")
        ),
        "n1",
    )?;
    let named = run(at, &as_user("ferrix", None, "set-hostname nopass"), "n2")?;
    let refused = run(at, &as_user("guest", None, "set-hostname nopass2"), "n3")?;
    let names = names(at, "n4")?;
    if answer(&named, "settingsctl: ")
        != Some("settingsctl: done: the computer is called nopass (was box)")
        || answer(&refused, "settingsctl: ")
            != Some("settingsctl: refused: not-permitted: guest is not in wheel")
        || names.as_deref() != Some("nopass nopass")
    {
        failures.push(format!(
            "Methods=none for settings.hostname did not let ferrix in alone: {named:?} \
             {refused:?} names {names:?}"
        ));
    }
    let other = run(
        at,
        "echo 'not it' | authctl try gate-mechanism ferrix",
        "n5",
    )?;
    let asked = answer(&other, "authctl: ").is_some_and(|line| line.starts_with("authctl: failed"));
    if !asked {
        failures.push(format!(
            "Methods=none loosened gate-mechanism, which is not a settings service: {other:?}"
        ));
    }
    Ok(())
}

/// The audit lines are in `svc log settings.service`, and hold no password.
fn audit(at: &mut Watching<'_>, failures: &mut Vec<String>) -> Result<()> {
    let lines = run(
        at,
        &format!(
            "echo \"$m-refused $(svc log settings.service -n 200 | grep -c 'account=guest auth=- result=not-permitted') \
             au-password $(svc log settings.service -n 200 | grep -c 'account=ferrix auth=password result=done') \
             au-none $(svc log settings.service -n 200 | grep -c 'account=ferrix auth=none result=done') \
             au-secret $(svc log settings.service -n 200 | grep -c -e '{FERRIX_PASSWORD}' -e '{GUEST_PASSWORD}')\""
        ),
        "au",
    )?;
    match answer(&lines, "au-refused ") {
        Some(line)
            if !line.starts_with("au-refused 0 ")
                && !line.contains(" au-password 0 ")
                && !line.contains(" au-none 0 ")
                && line.ends_with(" au-secret 0") => {}
        other => failures.push(format!(
            "the audit lines are not every request without the passwords: {other:?}"
        )),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FERRIX_PASSWORD, GUEST_PASSWORD, NO_PASSWORD, WRONG_PASSWORD, as_user};

    /// Each password is typed inside single quotes inside double ones.
    #[test]
    fn the_gate_passwords_quote_in_a_shell() {
        for password in [FERRIX_PASSWORD, GUEST_PASSWORD, WRONG_PASSWORD] {
            assert!(
                !password.contains(['\'', '"', '\\', '$', '`']),
                "{password:?}"
            );
        }
        assert!(NO_PASSWORD.starts_with("[Service]\n"));
        assert_eq!(
            as_user("guest", None, "set-hostname x"),
            "su guest -c \"settingsctl set-hostname x </dev/null\""
        );
    }
}
