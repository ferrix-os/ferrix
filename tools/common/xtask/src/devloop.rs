//! `test-devloop`: a component is changed on Ferrix, from a clone to a push,
//! and the running system takes the change (`docs/DEV-ON-FERRIX.md`).
//!
//! What a developer at a Ferrix desktop does, with nothing of the host's but
//! the far end of the network:
//!
//! 1. **Clone.** git, the app, clones the fixture repository over HTTP
//!    through the gateway, from the stub `test-net` uses ([`net::Servers`]).
//! 2. **Build and test.** Cargo and rustc from the toolchain volume build
//!    the fixture and run its tests, one of which must panic and one of
//!    which catches a panic: a test program that cannot unwind ends at the
//!    first of them. Where the volume has rustfmt and clippy ([`lints`]),
//!    `cargo fmt --check` and `cargo clippy` follow, as `check` starts.
//!    Where the volume has helix ([`editor`]), `hx --health rust` must
//!    name rust-analyzer, and a language-server session ([`LSP`]) must get
//!    the fixture's symbols from it: what the editor asks first.
//! 3. **Run.** The program is installed and started as a service of init's,
//!    `greeter.service`, and answers a connection with its line.
//! 4. **Edit, build, test, commit, push.** One word of the source is
//!    changed, the build and the tests run again, and the commit is pushed
//!    to a second, bare repository.
//! 5. **Replace it live.** The new program is moved over the old and `svc
//!    restart` starts it; the same connection now gets the new line, from a
//!    new main pid, with the machine up throughout.
//!
//! # What runs where
//!
//! Init is pid 1, as on a desktop, and the walk is a service of its own,
//! `devloop.service`, whose script ([`SCRIPT`]) says one `devloop:` line a
//! step and powers the machine off at its end, whatever happened. The
//! toolchain is the rustc volume at `/data`, as `test-rustc` attaches it.
//! Unless `--interpreter glibc` is given, the volume's programs run on
//! ferrousli's loader and `libc.so.6`, as `run-compositor --everything`'s do
//! -- which is the libc a developer's `cargo test` meets there.
//!
//! The fixture ([`FIXTURE`]) is a library, a program and three tests, with
//! no dependency: the guest reaches this process's stubs and nothing else.
//! The push goes to a bare repository in the guest, since the stub serves
//! git's dumb protocol, which takes no push; a push over the network is
//! `docs/DEV-ON-FERRIX.md`'s to prove, with credentials.
//!
//! x86-64 on a Linux host: the volume holds x86-64 programs, and
//! `fetch-rustc-sysroot.sh` makes it there.

use std::path::Path;

use crate::args::Args;
use crate::paths::{self, Arch};
use crate::ports::{Content, File};
use crate::{Error, Result, apps, busybox, cargo, fat, init, initramfs, native, net, qemu, rustc};
use crate::{shell, zinc};

/// The fixture's files, by their path in its repository.
const FIXTURE: &[(&str, &str)] = &[
    ("Cargo.toml", include_str!("devloop/fixture/manifest.toml")),
    ("src/lib.rs", include_str!("devloop/fixture/lib.rs")),
    ("src/main.rs", include_str!("devloop/fixture/main.rs")),
];

/// The language-server session the walk runs where the volume has the
/// editor: an editor's first questions to rust-analyzer, on the fixture.
const LSP: &str = include_str!("devloop/lsp.py");

/// Where it goes in the image.
const LSP_PATH: &str = "etc/ferrix/devloop-lsp.py";

/// Its first commit's subject, which the guest prints after cloning.
const SUBJECT: &str = "greeter: say version one";

/// The subject of the commit the guest makes, which it prints from the
/// repository it pushed to.
const PUSHED: &str = "greeter: say version two";

/// What the service says before the edit and after it.
const BEFORE: &str = "greeter: hello from version one";
const AFTER: &str = "greeter: hello from version two";

/// The fixture's service, started by the walk once the program is built.
const GREETER_UNIT: &str = "[Unit]\n\
     Description=The development loop's fixture, built on this machine\n\
     \n\
     [Service]\n\
     ExecStart=/usr/local/bin/greeter\n";

/// The walk's own service: the script on the console, where the host reads
/// it.
const WALK_UNIT: &str = "[Unit]\n\
     Description=The development loop, walked once\n\
     \n\
     [Service]\n\
     ExecStart=/bin/busybox sh /etc/ferrix/devloop.sh\n\
     StandardOutput=console\n\
     StandardError=console\n";

/// Where the script goes in the image.
const SCRIPT_PATH: &str = "etc/ferrix/devloop.sh";

/// The search path that finds ferrousli's `libc.so.6` in `/lib` before the
/// volume's libraries.
const LIBRARY_PATH: &str = "/lib:/lib/x86_64-linux-gnu";

/// The walk. `@PORT@` is the HTTP stub's, `@REPO@` the path it serves the
/// fixture at, `@LIBRARY_PATH@` the line that puts ferrousli's library
/// first, or nothing, and `@LINT@` is [`LINT`], or nothing.
///
/// Every step that fails says which and ends the walk; the machine is
/// powered off either way, so a failure costs a boot and not the timeout.
/// The service's answer is asked for a few times, since `svc start` returns
/// when the program has been started, not when it listens.
const SCRIPT: &str = r#"export PATH=/data/rust/bin:/bin:/usr/bin:/data/usr/bin
export HOME=/tmp/home CARGO_HOME=/tmp/home/.cargo CARGO_TERM_COLOR=never
@LIBRARY_PATH@
say() { echo "devloop: $*"; }
fail() { say "FAILED at $*"; svc poweroff; exit 1; }
answer() {
    for try in 1 2 3 4 5 6 7 8 9 10; do
        line=$(nc 127.0.0.1 7700 2>/dev/null)
        [ -n "$line" ] && { echo "$line"; return 0; }
        sleep 1
    done
    return 1
}
main_pid() { svc status greeter.service | sed -n 's/^ *Main PID: *//p'; }

mkdir -p "$HOME" /usr/local/bin || fail "the home directory"
udhcpc -i eth0 -n -q -t 5 -T 2 || fail "the DHCP lease"
cd "$HOME" || fail "the home directory"
say "cargo is $(cargo -V)"

# 1. Clone. Seconds when it works; a clone that stalls is named within two
# minutes, not at the gate's timeout.
timeout 120 git clone -q http://10.0.2.2:@PORT@@REPO@ greeter || {
    # What the interface, its neighbours and its connections were when the
    # clone gave up: a stall is the network's, and this is where it shows.
    ip neigh
    netstat -tan
    cat /proc/net/dev
    ping -c 2 -W 2 10.0.2.2
    fail "the clone"
}
cd greeter || fail "the clone"
say "cloned $(git log -1 --format=%s)"

# 2. Build and test.
cargo build || fail "the first build"
cargo test || fail "the first tests"
say "tests passed (one)"
@LINT@
@EDITOR@

# 3. Run it as a service.
cp target/debug/greeter /usr/local/bin/greeter || fail "the install"
svc start greeter.service || fail "svc start"
before=$(answer) || fail "the service's first answer"
pid_before=$(main_pid)
say "before $before"

# 4. Edit, build, test, commit, push.
sed -i 's/version one/version two/' src/lib.rs || fail "the edit"
cargo build || fail "the second build"
cargo test || fail "the second tests"
say "tests passed (two)"
git config user.name "Ferrix Developer" || fail "git config"
git config user.email developer@ferrix.test || fail "git config"
git commit -q -a -m "greeter: say version two" || fail "the commit"
git init -q --bare /tmp/remote.git || fail "the remote"
git push -q /tmp/remote.git main || fail "the push"
say "pushed $(git --git-dir=/tmp/remote.git log -1 --format=%s main)"

# 5. Replace it live: the new program beside the old, moved over it, and the
# service started again.
cp target/debug/greeter /usr/local/bin/greeter.new || fail "the second install"
mv /usr/local/bin/greeter.new /usr/local/bin/greeter || fail "the second install"
svc restart greeter.service || fail "svc restart"
after=$(answer) || fail "the service's second answer"
pid_after=$(main_pid)
say "after $after"
if [ -n "$pid_before" ] && [ -n "$pid_after" ] && [ "$pid_before" != "$pid_after" ]; then
    say "the service's main pid changed"
else
    say "the service's main pid did not change: '$pid_before' then '$pid_after'"
fi
say "done"
svc poweroff
"#;

/// What the walk does between its first tests and the service's start when
/// the volume has rustfmt and clippy ([`lints`]): the two steps `cargo xtask
/// check` starts with.
const LINT: &str = r#"cargo fmt --check || fail "cargo fmt --check"
cargo clippy -- -D warnings || fail "cargo clippy"
say "fmt and clippy clean""#;

/// What the walk does after [`LINT`] where the volume has helix
/// ([`editor`]): helix must find rust-analyzer, and rust-analyzer must
/// answer an editor's first questions about the fixture.
const EDITOR: &str = r#"hx --health rust > /tmp/hx-health 2>&1 || { cat /tmp/hx-health; fail "hx --health rust"; }
grep 'rust-analyzer: ' /tmp/hx-health | grep -q '/bin/rust-analyzer' \
    || { cat /tmp/hx-health; fail "hx finding rust-analyzer"; }
say "hx finds rust-analyzer"
python3 /@LSP_PATH@ /data/rust/bin/rust-analyzer "$PWD" "$PWD/src/lib.rs" \
    || { cat /tmp/rust-analyzer.err; fail "the language server session"; }
say "rust-analyzer answered the editor""#;

/// What the walk says when [`EDITOR`]'s steps passed.
const EDITED: [&str; 2] = [
    "devloop: hx finds rust-analyzer",
    "devloop: rust-analyzer answered the editor",
];

/// What the walk says when [`LINT`]'s steps passed.
const LINTED: &str = "devloop: fmt and clippy clean";

/// Whether the toolchain volume has rustfmt and clippy, by the tree
/// `fetch-rustc-sysroot.sh` keeps beside it: the script fetches them since
/// 2026-10-11, and a volume from before then has neither. A volume whose
/// tree is gone cannot be judged, and is walked without them.
fn lints() -> bool {
    rustc::tree().is_ok_and(|tree| {
        ["cargo-fmt", "rustfmt", "cargo-clippy", "clippy-driver"]
            .iter()
            .all(|tool| tree.join("rust/bin").join(tool).is_file())
    })
}

/// Whether the toolchain volume has helix and rust-analyzer, by the tree
/// beside it, as [`lints`] judges: the script fetches both since
/// 2026-10-11.
fn editor() -> bool {
    rustc::tree().is_ok_and(|tree| {
        tree.join("helix/hx").is_file() && tree.join("rust/bin/rust-analyzer").is_file()
    })
}

/// Guest memory unless `--memory` says otherwise: the compiler's libraries,
/// and the clone and its target directory in `/tmp`.
const MEMORY: u32 = 4096;

/// Seconds to wait unless `--timeout` says otherwise. Four small compiles;
/// generous for `tcg`, where rustc runs emulated.
const TIMEOUT: u64 = 1800;

/// The script, for a stub listening on `port`, with [`LINT`]'s steps when
/// `lint` and [`EDITOR`]'s when `edit`.
fn script(port: u16, ferrousli: bool, lint: bool, edit: bool) -> String {
    let library_path = if ferrousli {
        format!("export LD_LIBRARY_PATH={LIBRARY_PATH}")
    } else {
        String::new()
    };
    SCRIPT
        .replace("@PORT@", &port.to_string())
        .replace("@REPO@", net::GIT_REPO_PATH)
        .replace("@LIBRARY_PATH@", &library_path)
        .replace("@LINT@", if lint { LINT } else { "" })
        .replace("@EDITOR@", if edit { EDITOR } else { "" })
        .replace("@LSP_PATH@", LSP_PATH)
}

/// A file of the image holding `text`.
fn text_file(path: &str, text: &str) -> File {
    File {
        path: path.to_owned(),
        mode: 0o644,
        content: Content::Bytes(text.as_bytes().to_vec()),
    }
}

/// What the image carries for the walk: the two units, the walk's wanted by
/// `multi-user.target`, the script, and the console's getty masked.
fn walk_files(script: &str) -> Vec<File> {
    vec![
        text_file("etc/ferrix/units/greeter.service", GREETER_UNIT),
        text_file("etc/ferrix/units/devloop.service", WALK_UNIT),
        File {
            path: "etc/ferrix/units/multi-user.target.wants/devloop.service".to_owned(),
            mode: 0o777,
            content: Content::Link("/etc/ferrix/units/devloop.service".to_owned()),
        },
        text_file(SCRIPT_PATH, script),
        text_file(LSP_PATH, LSP),
        // No shell on the console, as on a desktop that wants none: its
        // prompt would be drawn into the walk's lines.
        File {
            path: "etc/ferrix/units/getty@.service".to_owned(),
            mode: 0o777,
            content: Content::Link("/dev/null".to_owned()),
        },
    ]
}

/// The links to the toolchain on the volume, and ferrousli's loader and
/// `libc.so.6` in glibc's place when the walk runs on it.
fn toolchain_files(arch: Arch, args: &Args, ferrousli: bool) -> Result<Vec<File>> {
    if !ferrousli {
        return Ok(rustc::files(rustc::DEFAULT_LINKS));
    }
    // The loader takes `/lib64`'s place, so the link to the volume's goes.
    let kept: Vec<_> = rustc::DEFAULT_LINKS
        .iter()
        .copied()
        .filter(|(path, _)| *path != "lib64")
        .collect();
    let mut files = rustc::files(&kept);
    files.extend(shell::carried_for(
        arch,
        &rustc::tree()?.join("rust/bin/rustc"),
        args,
    )?);
    Ok(files)
}

/// The two gates of working on Ferrix from within it: `test-devloop`, and
/// `test-selfhost`, whose build of the whole image is the loop's long way
/// round.
///
/// # Errors
///
/// As [`test_devloop`] and [`crate::selfhost::test_selfhost`].
pub(crate) fn run(command: &str, args: &Args) -> Result<()> {
    if command == "test-devloop" {
        test_devloop(args)
    } else {
        crate::selfhost::test_selfhost(args)
    }
}

/// Boot Ferrix and walk the development loop there.
///
/// # Errors
///
/// When the volume or git is missing, the image cannot be built, the boot
/// fails, or any step of the walk does not do what it must.
fn test_devloop(args: &Args) -> Result<()> {
    let arch = Arch::X86_64;
    if args.arches()?.iter().any(|&asked| asked != arch) {
        return Err(Error::new(
            "test-devloop runs on x86-64 only: the toolchain volume holds x86-64 programs",
        ));
    }
    let mut args = args.clone();
    args.data_image = Some(rustc::volume()?);
    args.net = true;
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }
    // The desktop's libc unless glibc is asked for by name.
    let ferrousli = args.interpreter.as_deref() != Some("glibc");
    if ferrousli {
        if args.interpreter.is_none() {
            args.interpreter = Some(shell::FERROUSLI.to_owned());
        }
        if args.libraries.is_empty() {
            args.libraries = vec![shell::FERROUSLI.to_owned()];
        }
    } else {
        args.interpreter = None;
    }

    let directory = paths::workspace_root().join("build").join("devloop-git");
    let repository = net::write_repository_of(&directory, FIXTURE, SUBJECT)?;
    let servers = net::Servers::start_serving(repository)?;
    println!("  host: {}", servers.describe());
    args.resolver = Some(servers.dns());

    let ports = apps::ported(arch, &args)?;
    if !ports.iter().any(|file| file.path == "usr/bin/git") {
        return Err(Error::new(
            "test-devloop needs git in the image: `cargo xtask build-apps --arch x86_64 --app \
             curl --app git`",
        ));
    }
    let mut files = init::carried(arch)?;
    if files.is_empty() {
        return Err(Error::new(format!("init is not built for {arch}")));
    }
    files.extend(ports);
    files.extend(toolchain_files(arch, &args, ferrousli)?);
    let (lint, edit) = volume_steps();
    files.extend(walk_files(&script(
        servers.http().port(),
        ferrousli,
        lint,
        edit,
    )));

    let libc = if ferrousli { "ferrousli" } else { "glibc" };
    println!("  {arch}: building an image whose init walks the development loop on {libc}");
    let shell =
        zinc::built(arch)?.ok_or_else(|| Error::new("zinc could not be built for x86-64"))?;
    let shell = read(&shell)?;
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel = cargo::build_kernel(arch, args.release)?;
    let natives = native::build(arch, args.release)?;
    let busybox = busybox::program(arch)?;
    let archive = initramfs::build(Some(&busybox), &natives, Some(&shell), &files)?;
    let options = init::command_line();
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, Some(&options))?;

    println!(
        "  {arch}: cloning, building, testing, replacing and pushing on Ferrix with {} MiB \
         (timeout {}s)",
        args.memory, args.timeout
    );
    let lines = qemu::watch_to_power_off(arch, &image, &kernel, &args, qemu::SUCCESS_MARKER)?;
    drop(servers);
    judge(&lines, lint, edit).map_err(|failure| {
        Error::new(format!(
            "{arch}: the development loop did not close: {failure}\n  Serial output is in {}",
            paths::build_dir(arch).join("serial.log").display()
        ))
    })?;
    println!("  {arch}: {}", closed_line(lint, edit));
    Ok(())
}

/// Which of the volume's own steps the walk takes, [`LINT`]'s and
/// [`EDITOR`]'s, saying of each it cannot take why.
fn volume_steps() -> (bool, bool) {
    let lint = lints();
    if !lint {
        println!(
            "  warning: the toolchain volume has no rustfmt or clippy, so the walk goes without \
             `cargo fmt --check` and `cargo clippy`; it is from before the script fetched them \
             -- run tools/common/fetch/fetch-rustc-sysroot.sh again"
        );
    }
    let edit = editor();
    if !edit {
        println!(
            "  warning: the toolchain volume has no helix, so the walk goes without the editor's \
             steps; it is from before the script fetched it -- run \
             tools/common/fetch/fetch-rustc-sysroot.sh again"
        );
    }
    (lint, edit)
}

/// What a walk that closed did.
fn closed_line(lint: bool, edit: bool) -> String {
    format!(
        "Ferrix cloned the fixture, built and tested it{}{}, ran it as a service, changed it, \
         pushed the change, and replaced the running service with the new build",
        if lint {
            ", found it formatted and free of clippy's warnings"
        } else {
            ""
        },
        if edit {
            ", had helix find rust-analyzer and rust-analyzer answer an editor about it"
        } else {
            ""
        }
    )
}

/// A file's bytes, or an error naming it.
fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| Error::new(format!("reading {}: {error}", path.display())))
}

/// What the walk says, in the order it must say it; [`LINTED`] among it
/// when the walk linted, and [`EDITED`] when it asked the editor.
fn expected(lint: bool, edit: bool) -> Vec<String> {
    let mut lines = vec![
        format!("devloop: cloned {SUBJECT}"),
        "devloop: tests passed (one)".to_owned(),
    ];
    if lint {
        lines.push(LINTED.to_owned());
    }
    if edit {
        lines.extend(EDITED.iter().map(|line| (*line).to_owned()));
    }
    lines.extend([
        format!("devloop: before {BEFORE}"),
        "devloop: tests passed (two)".to_owned(),
        format!("devloop: pushed {PUSHED}"),
        format!("devloop: after {AFTER}"),
        "devloop: the service's main pid changed".to_owned(),
        "devloop: done".to_owned(),
    ]);
    lines
}

/// Whether the transcript is a walk that closed: every line of
/// [`expected`], in order, after the boot marker, and no step that failed.
/// The first thing wrong is the error.
fn judge(lines: &[String], lint: bool, edit: bool) -> std::result::Result<(), String> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    let said: Vec<&str> = after_boot
        .iter()
        .map(|line| line.trim())
        .filter(|line| line.starts_with("devloop: "))
        .collect();
    if let Some(failed) = said.iter().find(|line| line.starts_with("devloop: FAILED")) {
        return Err(format!("the walk said `{failed}`"));
    }
    let mut rest = said.iter();
    for want in expected(lint, edit) {
        if !rest.any(|line| *line == want) {
            let near = said
                .iter()
                .find(|line| {
                    want.split(' ')
                        .nth(1)
                        .is_some_and(|word| line.split(' ').nth(1) == Some(word))
                })
                .map_or_else(
                    || "nothing of the kind".to_owned(),
                    |line| format!("`{line}`"),
                );
            return Err(format!(
                "the walk never said `{want}`, or not in its place; it said {near}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A boot's lines, then the walk's.
    fn transcript(walk: &[&str]) -> Vec<String> {
        let mut lines = vec![format!("{} stages 1-12", qemu::SUCCESS_MARKER)];
        lines.extend(walk.iter().map(|line| (*line).to_owned()));
        lines
    }

    /// The walk as the guest says it when every step works.
    fn closed() -> Vec<String> {
        expected(false, false)
    }

    #[test]
    fn a_walk_that_says_every_step_in_order_closes() {
        let mut lines = transcript(&[]);
        lines.push("   Compiling greeter v0.1.0 (/tmp/home/greeter)".to_owned());
        lines.extend(closed());
        assert_eq!(judge(&lines, false, false), Ok(()));
    }

    #[test]
    fn a_service_still_saying_the_old_words_does_not_close() {
        let mut walk = closed();
        walk[5] = format!("devloop: after {BEFORE}");
        let mut lines = transcript(&[]);
        lines.extend(walk);
        let failure = judge(&lines, false, false).expect_err("the old words are not the new ones");
        assert!(failure.contains(&format!("never said `devloop: after {AFTER}`")));
        assert!(failure.contains(&format!("it said `devloop: after {BEFORE}`")));
    }

    #[test]
    fn a_failed_step_is_named() {
        let lines = transcript(&[
            "devloop: cloned greeter: say version one",
            "devloop: FAILED at the first tests",
        ]);
        let failure = judge(&lines, false, false).expect_err("a failed step fails the walk");
        assert!(failure.contains("FAILED at the first tests"));
    }

    #[test]
    fn a_main_pid_that_did_not_change_does_not_close() {
        let mut walk = closed();
        walk[6] = "devloop: the service's main pid did not change: '31' then '31'".to_owned();
        let mut lines = transcript(&[]);
        lines.extend(walk);
        assert!(
            judge(&lines, false, false)
                .expect_err("the same process is not a restart")
                .contains("main pid changed")
        );
    }

    #[test]
    fn a_walk_that_was_to_lint_and_did_not_does_not_close() {
        let mut lines = transcript(&[]);
        lines.extend(closed());
        assert!(
            judge(&lines, true, false)
                .expect_err("the lint line is missing")
                .contains(LINTED)
        );
        let mut lines = transcript(&[]);
        lines.extend(expected(true, false));
        assert_eq!(judge(&lines, true, false), Ok(()));
    }

    #[test]
    fn a_walk_that_was_to_ask_the_editor_and_did_not_does_not_close() {
        let mut lines = transcript(&[]);
        lines.extend(expected(true, false));
        assert!(
            judge(&lines, true, true)
                .expect_err("the editor's lines are missing")
                .contains(EDITED[0])
        );
        let mut lines = transcript(&[]);
        lines.extend(expected(true, true));
        assert_eq!(judge(&lines, true, true), Ok(()));
    }

    #[test]
    fn the_walk_carries_the_language_server_session() {
        let files = walk_files("true\n");
        assert!(files.iter().any(|file| file.path == LSP_PATH));
        assert!(LSP.contains("workspace/symbol") && LSP.contains("greeting"));
        let library = FIXTURE
            .iter()
            .find(|(path, _)| *path == "src/lib.rs")
            .map(|(_, text)| *text)
            .expect("the fixture has a library");
        assert!(
            library.contains("pub fn greeting()"),
            "the session asks for it"
        );
    }

    #[test]
    fn steps_out_of_order_do_not_close() {
        let mut walk = closed();
        walk.swap(2, 5);
        let mut lines = transcript(&[]);
        lines.extend(walk);
        assert!(judge(&lines, false, false).is_err());
    }

    #[test]
    fn lines_before_the_boot_marker_do_not_count() {
        let mut lines = closed();
        lines.push(format!("{} stages 1-12", qemu::SUCCESS_MARKER));
        assert!(judge(&lines, false, false).is_err());
    }

    #[test]
    fn the_script_carries_the_port_and_no_placeholder() {
        let on_ferrousli = script(4321, true, true, true);
        assert!(on_ferrousli.contains("git clone -q http://10.0.2.2:4321/repo.git greeter"));
        assert!(on_ferrousli.contains("export LD_LIBRARY_PATH=/lib:/lib/x86_64-linux-gnu"));
        let on_glibc = script(4321, false, false, false);
        assert!(!on_glibc.contains("LD_LIBRARY_PATH"));
        assert!(on_ferrousli.contains("cargo fmt --check") && !on_glibc.contains("cargo fmt"));
        assert!(on_ferrousli.contains("hx --health rust") && !on_glibc.contains("hx "));
        assert!(on_ferrousli.contains("python3 /etc/ferrix/devloop-lsp.py"));
        for placeholder in [
            "@PORT@",
            "@REPO@",
            "@LIBRARY_PATH@",
            "@LINT@",
            "@EDITOR@",
            "@LSP_PATH@",
        ] {
            assert!(!on_ferrousli.contains(placeholder) && !on_glibc.contains(placeholder));
        }
    }

    /// The script's words and the judge's are the same words: the fixture
    /// says [`BEFORE`], the script's edit turns it into [`AFTER`], and its
    /// commit is [`PUSHED`].
    #[test]
    fn the_fixture_the_script_and_the_judge_agree() {
        let library = FIXTURE
            .iter()
            .find(|(path, _)| *path == "src/lib.rs")
            .map(|(_, text)| *text)
            .expect("the fixture has a library");
        assert!(library.contains("pub const WORDS: &str = \"version one\";"));
        assert!(BEFORE.ends_with("version one") && AFTER.ends_with("version two"));
        assert!(SCRIPT.contains("sed -i 's/version one/version two/' src/lib.rs"));
        assert!(SCRIPT.contains(&format!("git commit -q -a -m \"{PUSHED}\"")));
        assert!(SCRIPT.contains("svc restart greeter.service"));
        let edited = library.replace("version one", "version two");
        assert!(edited.contains("pub const WORDS: &str = \"version two\";"));
    }

    #[test]
    fn the_walk_is_a_service_of_multi_user_target() {
        let files = walk_files("true\n");
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&"etc/ferrix/units/greeter.service"));
        assert!(paths.contains(&"etc/ferrix/units/multi-user.target.wants/devloop.service"));
        assert!(paths.contains(&SCRIPT_PATH));
        assert!(paths.contains(&"etc/ferrix/units/getty@.service"));
    }
}
