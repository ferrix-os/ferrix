//! `test-claude-code`: Anthropic's Claude Code on Ferrix, running a command
//! of its own in bash.
//!
//! The program is Claude Code's native linux-x64 release, not one built
//! here: a 234 MB glibc program -- a JavaScript runtime with Claude Code
//! bundled into it -- run by Debian's `ld-linux` on Debian's glibc, beside
//! Debian's bash and ripgrep, all of it on a btrfs volume
//! `tools/common/fetch/fetch-claude-code.sh` makes from pinned downloads.
//! `docs/CLAUDE-CODE.md` says what it needs and what is left.
//!
//! Two steps, the second a proof of much more than the first: `--version`,
//! which says the program loaded, its libraries resolved and its runtime
//! started; and one turn of `claude -p`, against a Messages API this process
//! serves ([`Api`]), in which the model asks for a Bash command, Claude Code
//! runs it in bash on Ferrix, and sends back what it printed. The reply the
//! guest prints is only written once that output has arrived, so the line
//! says the whole loop ran: HTTP over the network, the streaming reply
//! parsed, a tool call dispatched, a child process in a shell, and its
//! output read back.
//!
//! # No model, and no key
//!
//! `ANTHROPIC_BASE_URL` points Claude Code at [`Api`], on the host's loopback
//! through the gateway's `10.0.2.2`, as `test-net`'s servers are. It answers
//! the requests Claude Code makes with fixed replies, so the gate costs
//! nothing, needs no account, and says the same thing on a machine with no
//! internet. Its key is a made-up one [`Api`] requires. Claude Code's
//! nonessential traffic -- telemetry, error reports, the update check -- is
//! turned off by its own variable, so nothing else leaves the guest.
//!
//! # Where Claude Code lives
//!
//! As Chrome's does: the volume carries no `ferrix-root` label, so the kernel
//! mounts it at `/data`, under QEMU's `snapshot=on`, and the initramfs links
//! the paths glibc names into it ([`LINKS`]). busybox is on the image for
//! `udhcpc`; zinc runs the script; bash is the volume's, and `SHELL` names it,
//! since Claude Code's Bash tool will run commands in bash or zsh and in
//! nothing else.
//!
//! Claude Code is started as `claude`, [`WRAPPER`] at `/bin/claude`, which is
//! what the `--everything` desktop's terminals have too ([`desktop_files`]):
//! `everything.rs` merges this volume's tree into that desktop's. With
//! `--everything` the gate boots that merged volume in place of this one, so
//! what it passes is what the desktop runs.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::args::Args;
use crate::paths::Arch;
use crate::{Error, Result, busybox, cargo, fat, initramfs, native, qemu, rustc, shell, zinc};

/// glibc's paths, each a link into the volume: the loader where Claude Code's
/// and bash's `PT_INTERP` name it, and the directories it searches.
pub(crate) const LINKS: &[(&str, &str)] = &[
    ("lib64", "/data/usr/lib64"),
    ("lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
    ("usr/lib/x86_64-linux-gnu", "/data/usr/lib/x86_64-linux-gnu"),
];

/// [`LINKS`] for AArch64's volume, Debian's arm64: the loader where the
/// linux-arm64 build's and bash's `PT_INTERP` name it,
/// `/lib/ld-linux-aarch64.so.1`, and the directories it searches.
pub(crate) const ARM64_LINKS: &[(&str, &str)] = &[
    (
        "lib/ld-linux-aarch64.so.1",
        "/data/usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1",
    ),
    ("lib/aarch64-linux-gnu", "/data/usr/lib/aarch64-linux-gnu"),
    (
        "usr/lib/aarch64-linux-gnu",
        "/data/usr/lib/aarch64-linux-gnu",
    ),
];

/// The script: a lease for `eth0`, the version, and one prompt. `@PORT@` is
/// [`Api`]'s, `@KEY@` is [`KEY`], and `@PROMPT@` is [`PROMPT`].
///
/// `-p` reads what is piped to it as more of the prompt, until the end of its
/// input, whenever that input is not a terminal: the console is not one to
/// it, so its input is `/dev/null`.
const SCRIPT: &str = r#"export PATH=/bin:/data/usr/bin HOME=/tmp
udhcpc -i eth0 -n -q -t 5 -T 2 || exit 5
claude --version || exit 3
export ANTHROPIC_BASE_URL=http://10.0.2.2:@PORT@ ANTHROPIC_API_KEY=@KEY@
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
cd /tmp
claude -p '@PROMPT@' --allowedTools Bash --max-turns 3 < /dev/null || exit 4
printf '%s\n' '@CONFIG@' > /tmp/.claude.json
echo claude-code-gate: interactive
claude --allowedTools Bash
echo claude-code-gate: interactive ended
exit 18
"#;

/// What the script says just before it starts Claude Code's TUI on the
/// console, which the host then types at ([`drive_the_tui`]).
const INTERACTIVE: &str = "claude-code-gate: interactive";

/// What it says once the TUI has exited and the shell has the console back.
const INTERACTIVE_ENDED: &str = "claude-code-gate: interactive ended";

/// What is typed at the TUI: [`PROMPT`], so [`Api`] answers it with the same
/// tool call, and something of its own, so the typing shows in the log.
const TUI_PROMPT: &str = "ferrix-gate: run the check, typed at the TUI";

/// `~/.claude.json` before the TUI starts: onboarding done, the gate's key
/// approved -- Claude Code asks about a key from the environment once, by
/// its last 20 characters -- and `/tmp` trusted, so the TUI opens straight
/// on its prompt. `@KEYTAIL@` is those 20 characters.
const CONFIG: &str = r#"{"hasCompletedOnboarding":true,"theme":"dark","customApiKeyResponses":{"approved":["@KEYTAIL@"],"rejected":[]},"projects":{"/tmp":{"hasTrustDialogAccepted":true,"allowedTools":[]}}}"#;

/// Claude Code's program, under the tree `fetch-claude-code.sh` leaves
/// beside the volume.
const PROGRAM: &str = "claude-code/claude";

/// `/bin/claude`: Claude Code from the volume, with bash as the shell its
/// Bash tool runs commands in whatever shell started it -- the desktop's
/// terminals run zinc -- and its updater off, since it would replace a
/// program on a volume every boot starts afresh.
const WRAPPER: &str = r#"#!/bin/sh
[ -x /data/claude-code/claude ] || { echo "claude: /data/claude-code is not on this volume" >&2; exit 1; }
export SHELL=/data/usr/bin/bash DISABLE_AUTOUPDATER=1
exec /data/claude-code/claude "$@"
"#;

/// What an image carries for Claude Code: [`WRAPPER`] as `/bin/claude`,
/// and [`LINKS`] less any path `carried` already has -- Chrome's links on
/// the `--everything` desktop are the same ones -- nor a path the archive
/// already has files under, as `/lib64` when ferrousli's loader is in it.
pub(crate) fn desktop_files(carried: &[crate::ports::File]) -> Vec<crate::ports::File> {
    let taken = |path: &str| {
        carried.iter().any(|file| {
            file.path == path
                || file
                    .path
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    };
    let links: Vec<(&str, &str)> = LINKS
        .iter()
        .copied()
        .filter(|(path, _)| !taken(path))
        .collect();
    let mut files = rustc::files(&links);
    files.push(crate::ports::File {
        path: "bin/claude".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(WRAPPER.as_bytes().to_vec()),
    });
    // fuzzel's entry, so the launcher opens it in a terminal of its own.
    files.push(crate::ports::File {
        path: "usr/share/applications/claude.desktop".to_owned(),
        mode: 0o644,
        content: crate::ports::Content::Bytes(DESKTOP_ENTRY.as_bytes().to_vec()),
    });
    files
}

/// The launcher's entry for Claude Code: `claude` in a terminal, with the
/// terminal's icon.
const DESKTOP_ENTRY: &str = "[Desktop Entry]\nType=Application\nName=Claude Code\n\
     GenericName=Coding Agent\nComment=Anthropic's Claude Code, from the data disk\n\
     Exec=claude\nIcon=ferrix-terminal\nTerminal=true\nKeywords=claude;ai;code;agent;\n\
     Categories=Development;\n";

/// What `--version` prints, which says Claude Code and its runtime started.
const VERSION: &str = "2.1.280 (Claude Code)";

/// The status the script exits with when both steps succeeded.
const STATUS: i32 = 18;

/// The key the script gives Claude Code, and [`Api`] requires in every
/// request's `x-api-key`: made up, and only ever sent to this process.
const KEY: &str = "sk-ant-ferrix-gate-not-a-key";

/// The prompt: [`Api`] knows the request it answers with a tool call by it.
const PROMPT: &str = "ferrix-gate: run the check";

/// The command [`Api`] asks for. `$((6*7))` is bash's to work out, so its
/// output carries a number the command's text does not.
const COMMAND: &str = "echo ferrix-bash-$((6*7)); uname -s";

/// What that command prints on Ferrix, as the tool result's JSON carries it.
const COMMAND_OUTPUT: &str = r"ferrix-bash-42\nFerrix";

/// [`Api`]'s last reply, which the guest prints: sent only once
/// [`COMMAND_OUTPUT`] came back in a tool result.
const REPLY: &str = "claude-code-gate: bash on Ferrix said ferrix-bash-42";

/// Memory for the guest: the runtime's heap, and the program's pages.
const MEMORY: u32 = 2048;

/// Seconds for the boot.
const TIMEOUT: u64 = 900;

/// Where `tools/common/fetch/fetch-claude-code.sh` writes, unless
/// `FERRIX_CLAUDE_CODE_VOLUME` names another directory.
///
/// # Errors
///
/// The volume has not been fetched.
pub(crate) fn volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_CLAUDE_CODE_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("claude-code")?,
    };
    let image = directory.join("claude-code.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-claude-code.sh makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// Where `tools/common/fetch/fetch-claude-code.sh arm64` writes, unless
/// `FERRIX_CLAUDE_CODE_ARM64_VOLUME` names another directory: Claude Code's
/// linux-arm64 build on Debian's arm64 glibc and bash.
///
/// # Errors
///
/// The volume has not been fetched.
pub(crate) fn arm64_volume() -> Result<std::path::PathBuf> {
    let directory = match std::env::var_os("FERRIX_CLAUDE_CODE_ARM64_VOLUME") {
        Some(directory) => std::path::PathBuf::from(directory),
        None => crate::paths::volume_directory("claude-code-arm64")?,
    };
    let image = directory.join("claude-code.img");
    if !image.is_file() {
        return Err(Error::new(format!(
            "{} is not there: tools/common/fetch/fetch-claude-code.sh arm64 makes it",
            image.display()
        )));
    }
    Ok(image)
}

/// What an AArch64 image carries for Claude Code: [`ARM64_LINKS`] and
/// [`WRAPPER`] as `/bin/claude`. There is no desktop to merge into there.
fn arm64_files() -> Vec<crate::ports::File> {
    let mut files = rustc::files(ARM64_LINKS);
    files.push(crate::ports::File {
        path: "bin/claude".to_owned(),
        mode: 0o755,
        content: crate::ports::Content::Bytes(WRAPPER.as_bytes().to_vec()),
    });
    files
}

/// The script, for [`Api`] listening on `port`.
fn script(port: u16, ferrousli: bool) -> String {
    let tail = KEY.get(KEY.len().saturating_sub(20)..).unwrap_or(KEY);
    let script = SCRIPT
        .replace("@CONFIG@", &CONFIG.replace("@KEYTAIL@", tail))
        .replace("@PORT@", &port.to_string())
        .replace("@KEY@", KEY)
        .replace("@PROMPT@", PROMPT);
    if ferrousli {
        format!(
            "export LD_LIBRARY_PATH={}
{script}",
            crate::chrome::LIBRARY_PATH
        )
    } else {
        script
    }
}

/// What typing at the TUI came to.
#[derive(Debug, Default, Clone, Copy)]
struct Tui {
    /// The TUI drew its screen: the product's name and its shortcut hint.
    drew: bool,
    /// [`REPLY`] was drawn after the prompt was typed.
    replied: bool,
    /// The TUI exited and the shell went on.
    exited: bool,
}

/// How long the TUI is given to start, to answer, and to exit.
const TUI_PATIENCE: Duration = Duration::from_secs(120);

/// How many requests [`Api`] has answered with `what`.
fn answered(api: &Api, what: &str) -> usize {
    let suffix = format!("-> {what}");
    api.seen()
        .requests
        .iter()
        .filter(|request| request.ends_with(&suffix))
        .count()
}

/// Read the console until [`Api`] has answered `count` requests with `what`,
/// or [`TUI_PATIENCE`] has passed. Whether it did.
///
/// The TUI's screen is no help here: it places each word with a cursor
/// movement and ends no line, so the console reader sees none of it until
/// the shell's next line after it exits. What it asks the API for is the
/// sign of where it is.
fn wait_for_api(
    watching: &mut qemu::Watching<'_>,
    api: &Api,
    what: &str,
    count: usize,
) -> Result<bool> {
    let deadline = std::time::Instant::now() + TUI_PATIENCE;
    while std::time::Instant::now() < deadline {
        if answered(api, what) >= count {
            return Ok(true);
        }
        watching.read_what_was_said(Duration::from_millis(500))?;
    }
    Ok(answered(api, what) >= count)
}

/// Type at Claude Code's TUI once the script has started it: once it has
/// made its startup check of the API, the prompt and Enter; once the API
/// has sent its reply to the tool's run, `/exit` and Enter; then wait for
/// the shell to say it has the console back. What the screen showed is
/// judged afterwards, from all of it ([`screen_text`]).
fn drive_the_tui(watching: &mut qemu::Watching<'_>, api: &Api) -> Result<Tui> {
    // The `-p` turn made one reachability check and got one reply already.
    let _ = wait_for_api(watching, api, "hello", 2)?;
    // Past the check, the TUI still has its screen to draw and its input to
    // put in raw mode; a key typed before that is the line discipline's. A
    // plain pause, since the console reader sees the TUI as quiet the whole
    // time and would not wait at all; the lines keep until it looks.
    std::thread::sleep(Duration::from_secs(10));
    watching.type_in(TUI_PROMPT.as_bytes())?;
    watching.read_what_was_said(Duration::from_secs(1))?;
    watching.type_in(b"\r")?;
    let _ = wait_for_api(watching, api, "reply", 2)?;
    // Time to draw the reply, which the console reader cannot see either.
    std::thread::sleep(Duration::from_secs(3));
    watching.type_in(b"/exit")?;
    watching.read_what_was_said(Duration::from_secs(1))?;
    watching.type_in(b"\r")?;
    let exited = watching.read_more(std::time::Instant::now() + TUI_PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(INTERACTIVE_ENDED))
    })?;
    let _ = watching.read_more(std::time::Instant::now() + TUI_PATIENCE, |lines| {
        lines.iter().any(|line| line.contains(shell::EXITED))
    })?;
    let screen = screen_text(watching.after());
    Ok(Tui {
        drew: screen.contains("ClaudeCode") && screen.contains("?forshortcuts"),
        replied: screen.contains(&squeeze(REPLY)),
        exited,
    })
}

/// `text` without white space: the TUI moves the cursor between words
/// rather than print the spaces.
fn squeeze(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// What the console showed, as text: every line joined, each escape
/// sequence -- `CSI` ones ending in a letter, `OSC` ones ending in `BEL` --
/// and every other control character taken out, and [`squeeze`]d.
fn screen_text(lines: &[String]) -> String {
    let mut text = String::new();
    let mut chars = lines.iter().flat_map(|line| line.chars()).peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if !c.is_control() && !c.is_whitespace() {
                text.push(c);
            }
            continue;
        }
        match chars.next() {
            Some('[') => {
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() || c == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                for c in chars.by_ref() {
                    if c == '\u{7}' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    text
}

/// Boot a shell whose script runs Claude Code against [`Api`].
///
/// # Errors
///
/// When the volume is missing, the image cannot be built, the boot fails, or
/// either step does not do what it must.
pub(crate) fn test_claude_code(args: &Args) -> Result<()> {
    let arch = match args.arches()?.as_slice() {
        [Arch::X86_64] => Arch::X86_64,
        [Arch::AArch64] => Arch::AArch64,
        _ => {
            return Err(Error::new(
                "test-claude-code runs on x86-64 (Claude Code's linux-x64 build) or AArch64 \
                 (its linux-arm64 build), one at a time",
            ));
        }
    };
    let arm64 = arch == Arch::AArch64;
    if arm64 && (args.everything || args.interpreter.is_some()) {
        return Err(Error::new(
            "test-claude-code on AArch64 boots its own volume on Debian's glibc: \
             --everything and --interpreter are x86-64's",
        ));
    }
    let mut args = args.clone();
    // Only the desktop's volume, and its release build, from what
    // `--everything` stands for: this boot has a console and no screen.
    args.display = false;
    args.gl = false;
    args.clipboard = false;
    args.chrome = false;
    args.data_image = Some(if arm64 {
        arm64_volume()?
    } else if args.everything {
        // Only there to say so when Claude Code has not been fetched, rather
        // than boot a desktop's volume without it.
        let _ = volume()?;
        crate::everything::volume()?
    } else {
        volume()?
    });
    // The `--everything` desktop runs the volume's glibc programs on
    // ferrousli's loader, in `/lib64`, and its `libc.so.6`, unless
    // `--interpreter glibc` asks for the volume's own
    // (`compositor::browser::chrome_libc`); so does this boot. It is what
    // `claude` meets in that desktop's terminals.
    let ferrousli = args.everything && args.interpreter.as_deref() != Some("glibc");
    if ferrousli && !crate::chrome::on_ferrousli(&args) {
        args.interpreter = Some(shell::FERROUSLI.to_owned());
        args.libraries = vec![shell::FERROUSLI.to_owned()];
    }
    let carried = if ferrousli {
        crate::chrome::ferrousli_loader(arch, &volume()?, PROGRAM, &args)?
    } else {
        Vec::new()
    };
    args.net = true;
    if !args.memory_given {
        args.memory = MEMORY;
    }
    if !args.timeout_given {
        args.timeout = TIMEOUT;
    }

    let api = Api::start()?;
    let shell = zinc::built(arch)?
        .ok_or_else(|| Error::new(format!("zinc could not be built for {arch}")))?;
    let on = if args.everything {
        "the --everything desktop's volume"
    } else {
        "its own volume"
    };
    let libc = if ferrousli {
        "ferrousli's loader and libc.so.6"
    } else {
        "glibc"
    };
    println!("  {arch}: building an image whose shell runs Claude Code on {libc}, from {on}");
    let loader = cargo::build_loader(arch, args.release)?;
    let kernel =
        cargo::build_kernel_with_init(arch, args.release, &shell, &script(api.port, ferrousli))?;
    let natives = native::build(arch, args.release)?;
    let bytes = std::fs::read(&shell)
        .map_err(|error| Error::new(format!("reading {}: {error}", shell.display())))?;
    // The project's busybox for `udhcpc` and the `ip` its lease script runs;
    // zinc stays the shell.
    let busybox = busybox::program(arch)?;
    let mut files = if arm64 {
        arm64_files()
    } else {
        desktop_files(&carried)
    };
    files.extend(carried);
    let archive = initramfs::build(Some(&busybox), &natives, Some(&bytes), &files)?;
    let image = fat::write_image_with(arch, &loader, &kernel, &archive, None)?;

    println!(
        "  {arch}: running Claude Code on Ferrix with {} MiB (timeout {}s), its API on port {}",
        args.memory, args.timeout, api.port
    );
    let mut tui = Tui::default();
    let lines = qemu::watch_then(arch, &image, &kernel, &args, INTERACTIVE, |watching| {
        tui = drive_the_tui(watching, &api)?;
        Ok(())
    })?;
    let seen = api.stop();
    judge(arch, &lines, &seen)?;
    judge_tui(arch, tui, &seen)
}

/// Whether typing at the TUI did what a person at the terminal would see
/// it do.
fn judge_tui(arch: Arch, tui: Tui, seen: &Seen) -> Result<()> {
    let requests = seen.requests.join("\n    ");
    match (tui.drew, tui.replied, tui.exited) {
        (true, true, true) => {
            println!(
                "  {arch}: Claude Code's TUI took a prompt typed at the console, showed the \
                 answer to its tool's run, and exited on /exit"
            );
            Ok(())
        }
        (false, ..) => Err(Error::new(format!(
            "{arch}: Claude Code's TUI drew nothing on the console"
        ))),
        (true, false, _) => Err(Error::new(format!(
            "{arch}: the prompt typed at Claude Code's TUI got no answer on the screen; the API \
             saw:\n    {requests}"
        ))),
        (true, true, false) => Err(Error::new(format!(
            "{arch}: Claude Code's TUI answered, and did not exit on /exit"
        ))),
    }
}

/// Whether the transcript is a Claude Code that started, and a turn whose
/// tool call ran on Ferrix and came back.
fn judge(arch: Arch, lines: &[String], seen: &Seen) -> Result<()> {
    let after_boot = lines
        .iter()
        .position(|line| line.contains(qemu::SUCCESS_MARKER))
        .and_then(|at| lines.get(at..))
        .unwrap_or_default();
    // Anywhere in its line: Claude Code leaves the terminal with the cursor
    // shown again, an escape the kernel's next line follows on the same one.
    let exited = after_boot
        .iter()
        .find_map(|line| line.split_once(shell::EXITED))
        .map(|(_, status)| status.trim());
    let loaded = after_boot.iter().any(|line| line.contains(VERSION));
    let replied = after_boot.iter().any(|line| line.trim_end() == REPLY);
    let requests = seen.requests.join("\n    ");
    match exited {
        Some(status) if status == STATUS.to_string() && loaded && replied && seen.tool_ran => {
            println!(
                "  {arch}: Claude Code started ({VERSION}), and ran a command in bash on Ferrix \
                 for its model ({} requests)",
                seen.requests.len()
            );
            Ok(())
        }
        Some("5") => Err(Error::new(format!("{arch}: eth0 got no DHCP lease"))),
        Some("3") => Err(Error::new(format!("{arch}: `claude --version` failed"))),
        Some("4") => Err(Error::new(format!(
            "{arch}: Claude Code started, and `claude -p` failed; the API saw:\n    {requests}"
        ))),
        Some(status) => Err(Error::new(format!(
            "{arch}: the script exited with {status}; version {loaded}, reply {replied}, \
             tool result {}; the API saw:\n    {requests}",
            seen.tool_ran
        ))),
        None => Err(Error::new(format!("{arch}: the shell never exited"))),
    }
}

/// What [`Api`] was asked, for the verdict and for a failure's report.
#[derive(Debug, Default, Clone)]
struct Seen {
    /// Each request's line, and what was answered.
    requests: Vec<String>,
    /// Whether a request carried [`COMMAND_OUTPUT`] back as a tool result.
    tool_ran: bool,
}

/// A Messages API with fixed answers, on the host's loopback.
///
/// A request carrying [`PROMPT`] and tools is answered with a call of the
/// Bash tool running [`COMMAND`]; one carrying that command's output with
/// [`REPLY`]; any other -- a side request of Claude Code's own, such as a
/// title for the conversation -- with a word, so it has an answer.
#[derive(Debug)]
struct Api {
    /// The port, which the guest reaches as `10.0.2.2`'s.
    port: u16,
    /// What was asked.
    seen: Arc<Mutex<Seen>>,
    /// Set to stop the listening thread.
    stop: Arc<AtomicBool>,
    /// The listening thread.
    thread: Option<JoinHandle<()>>,
}

impl Api {
    /// Bind on the loopback and start answering.
    fn start() -> Result<Api> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|error| Error::new(format!("could not bind the API stub: {error}")))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let seen = Arc::clone(&seen);
            let stop = Arc::clone(&stop);
            std::thread::Builder::new()
                .name("ferrix-claude-api".to_owned())
                .spawn(move || listen(&listener, &stop, &seen))?
        };
        Ok(Api {
            port,
            seen,
            stop,
            thread: Some(thread),
        })
    }

    /// Stop answering, and say what was asked.
    /// What has been asked so far.
    fn seen(&self) -> Seen {
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }

    fn stop(mut self) -> Seen {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default()
    }
}

/// Accept connections until told to stop, each answered on a thread of its
/// own: a client may hold one connection open idle while it makes another.
fn listen(listener: &TcpListener, stop: &AtomicBool, seen: &Arc<Mutex<Seen>>) {
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                let seen = Arc::clone(seen);
                let _ = std::thread::Builder::new()
                    .name("ferrix-claude-conn".to_owned())
                    .spawn(move || {
                        let _ = answer(stream, &seen);
                    });
            }
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// One request read whole: its line, its headers, and its body.
#[derive(Debug)]
struct Request {
    /// `POST /v1/messages?beta=true`, and the like.
    line: String,
    /// Header names, lowercased, and values.
    headers: Vec<(String, String)>,
    /// The body, decoded from chunks if it came in them.
    body: String,
}

impl Request {
    /// The value of header `name`, which is lowercase.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// The path, without its query.
    fn path(&self) -> &str {
        let target = self.line.split(' ').nth(1).unwrap_or_default();
        target.split('?').next().unwrap_or_default()
    }
}

/// Read one request from `stream`, answer it, and close.
fn answer(mut stream: TcpStream, seen: &Mutex<Seen>) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let request = read_request(&mut stream)?;
    let (status, content_type, body, what) = respond(&request);
    if let Ok(mut seen) = seen.lock() {
        seen.requests.push(format!("{} -> {what}", request.line));
        if what == "reply" {
            seen.tool_ran = true;
        }
    }
    let head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n\
         request-id: req_ferrix\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    // A `HEAD` is answered with the length the body would have, and no body.
    if !request.line.starts_with("HEAD ") {
        stream.write_all(body.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

/// Read a request's head, then its body by `content-length` or by chunks.
fn read_request(stream: &mut TcpStream) -> Result<Request> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16384];
    let end = loop {
        if let Some(at) = find(&bytes, b"\r\n\r\n") {
            break at;
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(Error::new("the connection closed inside a request's head"));
        }
        bytes.extend_from_slice(buffer.get(..count).unwrap_or_default());
    };
    let head = String::from_utf8_lossy(bytes.get(..end).unwrap_or_default()).into_owned();
    let mut rest = bytes.get(end + 4..).unwrap_or_default().to_vec();
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let mut request = Request {
        line,
        headers,
        body: String::new(),
    };
    let chunked = request
        .header("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        read_chunks(stream, &mut rest)?
    } else {
        let length: usize = request
            .header("content-length")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        while rest.len() < length {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            rest.extend_from_slice(buffer.get(..count).unwrap_or_default());
        }
        rest.truncate(length);
        rest
    };
    request.body = String::from_utf8_lossy(&body).into_owned();
    Ok(request)
}

/// A chunked body: `rest` holds what came after the head, and more is read
/// until the zero-length chunk.
fn read_chunks(stream: &mut TcpStream, rest: &mut Vec<u8>) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut buffer = [0u8; 16384];
    loop {
        if let Some(at) = find(rest, b"\r\n") {
            let size_text =
                String::from_utf8_lossy(rest.get(..at).unwrap_or_default()).into_owned();
            let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("0").trim(), 16)
                .map_err(|_| Error::new(format!("a chunk size of {size_text:?}")))?;
            if size == 0 {
                return Ok(body);
            }
            let needed = at + 2 + size + 2;
            if rest.len() >= needed {
                body.extend_from_slice(rest.get(at + 2..at + 2 + size).unwrap_or_default());
                let _ = rest.drain(..needed);
                continue;
            }
        }
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Err(Error::new("the connection closed inside a chunked body"));
        }
        rest.extend_from_slice(buffer.get(..count).unwrap_or_default());
    }
}

/// Where `needle` first starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The status, content type, body and a word for the log, for `request`.
fn respond(request: &Request) -> (&'static str, &'static str, String, &'static str) {
    // Claude Code's check that the API can be reached at all, which it makes
    // without a key before its first request.
    if request.path() == "/api/hello" {
        return ("200 OK", "application/json", "{}".to_owned(), "hello");
    }
    if request.header("x-api-key") != Some(KEY) {
        return (
            "401 Unauthorized",
            "application/json",
            error_body("authentication_error", "the gate's key was not sent"),
            "refused: no key",
        );
    }
    match request.path() {
        "/v1/messages" => {
            let (blocks, stop_reason, what) = if request.body.contains(COMMAND_OUTPUT) {
                (vec![Block::Text(REPLY)], "end_turn", "reply")
            } else if request.body.contains(PROMPT) && request.body.contains("\"tools\"") {
                (vec![Block::Bash(COMMAND)], "tool_use", "tool call")
            } else {
                (vec![Block::Text("ok")], "end_turn", "side request")
            };
            let streamed = request.body.contains("\"stream\":true");
            if streamed {
                let body = stream_body(&blocks, stop_reason);
                ("200 OK", "text/event-stream", body, what)
            } else {
                let body = message_body(&blocks, stop_reason);
                ("200 OK", "application/json", body, what)
            }
        }
        "/v1/messages/count_tokens" => (
            "200 OK",
            "application/json",
            "{\"input_tokens\":1}".to_owned(),
            "counted",
        ),
        _ => (
            "404 Not Found",
            "application/json",
            error_body("not_found_error", "the gate serves messages only"),
            "not found",
        ),
    }
}

/// One content block of a reply.
#[derive(Debug, Clone, Copy)]
enum Block {
    /// Text.
    Text(&'static str),
    /// A call of the Bash tool with this command.
    Bash(&'static str),
}

/// The id of the one tool call.
const TOOL_ID: &str = "toolu_ferrix_gate";

/// The model the replies say they are from.
const MODEL: &str = "claude-ferrix-gate";

impl Block {
    /// The tool call's input, as JSON.
    fn input(command: &str) -> String {
        format!(
            "{{\"command\":{},\"description\":\"Say hello from Ferrix\"}}",
            json_string(command)
        )
    }

    /// The block whole, for a reply that is not streamed.
    fn whole(self) -> String {
        match self {
            Block::Text(text) => format!("{{\"type\":\"text\",\"text\":{}}}", json_string(text)),
            Block::Bash(command) => format!(
                "{{\"type\":\"tool_use\",\"id\":\"{TOOL_ID}\",\"name\":\"Bash\",\"input\":{}}}",
                Block::input(command)
            ),
        }
    }

    /// The block as it starts in a stream, empty.
    fn start(self) -> String {
        match self {
            Block::Text(_) => "{\"type\":\"text\",\"text\":\"\"}".to_owned(),
            Block::Bash(_) => {
                format!(
                    "{{\"type\":\"tool_use\",\"id\":\"{TOOL_ID}\",\"name\":\"Bash\",\"input\":{{}}}}"
                )
            }
        }
    }

    /// The block's content as one delta.
    fn delta(self) -> String {
        match self {
            Block::Text(text) => {
                format!("{{\"type\":\"text_delta\",\"text\":{}}}", json_string(text))
            }
            Block::Bash(command) => format!(
                "{{\"type\":\"input_json_delta\",\"partial_json\":{}}}",
                json_string(&Block::input(command))
            ),
        }
    }
}

/// A reply that is not streamed.
fn message_body(blocks: &[Block], stop_reason: &str) -> String {
    let content: Vec<String> = blocks.iter().map(|block| block.whole()).collect();
    format!(
        "{{\"id\":\"msg_ferrix_gate\",\"type\":\"message\",\"role\":\"assistant\",\
         \"model\":\"{MODEL}\",\"content\":[{}],\"stop_reason\":\"{stop_reason}\",\
         \"stop_sequence\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}",
        content.join(",")
    )
}

/// A reply as server-sent events, as the API streams one.
fn stream_body(blocks: &[Block], stop_reason: &str) -> String {
    let mut body = String::new();
    let mut event = |name: &str, data: String| {
        body.push_str(&format!("event: {name}\ndata: {data}\n\n"));
    };
    event(
        "message_start",
        format!(
            "{{\"type\":\"message_start\",\"message\":{{\"id\":\"msg_ferrix_gate\",\
             \"type\":\"message\",\"role\":\"assistant\",\"model\":\"{MODEL}\",\"content\":[],\
             \"stop_reason\":null,\"stop_sequence\":null,\
             \"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}"
        ),
    );
    for (index, block) in blocks.iter().enumerate() {
        event(
            "content_block_start",
            format!(
                "{{\"type\":\"content_block_start\",\"index\":{index},\"content_block\":{}}}",
                block.start()
            ),
        );
        event(
            "content_block_delta",
            format!(
                "{{\"type\":\"content_block_delta\",\"index\":{index},\"delta\":{}}}",
                block.delta()
            ),
        );
        event(
            "content_block_stop",
            format!("{{\"type\":\"content_block_stop\",\"index\":{index}}}"),
        );
    }
    event(
        "message_delta",
        format!(
            "{{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop_reason}\",\
             \"stop_sequence\":null}},\"usage\":{{\"output_tokens\":1}}}}"
        ),
    );
    event("message_stop", "{\"type\":\"message_stop\"}".to_owned());
    body
}

/// An API error's body.
fn error_body(kind: &str, message: &str) -> String {
    format!(
        "{{\"type\":\"error\",\"error\":{{\"type\":\"{kind}\",\"message\":{}}}}}",
        json_string(message)
    )
}

/// `text` as a JSON string, quoted and escaped.
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if u32::from(control) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", u32::from(control)));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transcript(text: &[&str]) -> Vec<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    fn request(path: &str, body: &str) -> Request {
        Request {
            line: format!("POST {path}?beta=true HTTP/1.1"),
            headers: vec![("x-api-key".to_owned(), KEY.to_owned())],
            body: body.to_owned(),
        }
    }

    fn ran() -> Seen {
        Seen {
            requests: vec!["POST /v1/messages -> reply".to_owned()],
            tool_ran: true,
        }
    }

    #[test]
    fn the_script_carries_the_port_key_and_prompt_and_no_placeholder() {
        let script = script(4242, false);
        assert!(script.contains("ANTHROPIC_BASE_URL=http://10.0.2.2:4242 "));
        assert!(script.contains(&format!("ANTHROPIC_API_KEY={KEY}\n")));
        assert!(script.contains(&format!("-p '{PROMPT}'")));
        assert!(!script.contains('@'));
        // The script must not already hold what the reply is required to.
        assert!(!script.contains("ferrix-bash-42"));
        assert!(!script.contains("LD_LIBRARY_PATH"));
        // On ferrousli the search path comes first, as the desktop's
        // compositor gives it to what it starts.
        let on_ferrousli = super::script(4242, true);
        assert!(on_ferrousli.starts_with("export LD_LIBRARY_PATH=/lib:/lib/x86_64-linux-gnu\n"));
        assert!(on_ferrousli.ends_with(&script));
    }

    #[test]
    fn the_prompt_with_tools_is_answered_with_the_bash_call() {
        let asked = request(
            "/v1/messages",
            &format!("{{\"stream\":true,\"messages\":[\"{PROMPT}\"],\"tools\":[]}}"),
        );
        let (status, content_type, body, what) = respond(&asked);
        assert_eq!(
            (status, content_type, what),
            ("200 OK", "text/event-stream", "tool call")
        );
        assert!(body.contains("\"name\":\"Bash\""));
        assert!(body.contains(r#"\"command\":\"echo ferrix-bash-$((6*7)); uname -s\""#));
        assert!(body.contains("\"stop_reason\":\"tool_use\""));
        assert!(body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));
    }

    #[test]
    fn the_command_output_is_answered_with_the_reply() {
        let asked = request(
            "/v1/messages",
            &format!("{{\"stream\":true,\"content\":\"{COMMAND_OUTPUT}\"}}"),
        );
        let (_, _, body, what) = respond(&asked);
        assert_eq!(what, "reply");
        assert!(body.contains(&json_string(REPLY)));
    }

    #[test]
    fn a_request_without_the_key_is_refused() {
        let mut asked = request("/v1/messages", PROMPT);
        asked.headers.clear();
        assert_eq!(respond(&asked).0, "401 Unauthorized");
    }

    #[test]
    fn the_wrapper_is_carried_and_links_are_not_carried_twice() {
        let alone = desktop_files(&[]);
        let paths: Vec<&str> = alone.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&"bin/claude") && paths.contains(&"lib64"));
        assert!(paths.contains(&"usr/share/applications/claude.desktop"));
        assert!(
            DESKTOP_ENTRY.contains("\nExec=claude\n") && DESKTOP_ENTRY.contains("Terminal=true")
        );
        let beside = desktop_files(&rustc::files(&[("lib64", "/data/usr/lib64")]));
        assert!(!beside.iter().any(|file| file.path == "lib64"));
        // ferrousli's loader in `/lib64` on the `--everything` desktop: a
        // link over that directory stops the kernel unpacking the archive.
        let loader = crate::ports::File {
            path: "lib64/ld-linux-x86-64.so.2".to_owned(),
            mode: 0o755,
            content: crate::ports::Content::Bytes(Vec::new()),
        };
        let with_loader = desktop_files(&[loader]);
        assert!(!with_loader.iter().any(|file| file.path == "lib64"));
        assert!(WRAPPER.contains("SHELL=/data/usr/bin/bash"));
        assert!(WRAPPER.contains("exec /data/claude-code/claude \"$@\""));
    }

    #[test]
    fn the_arm64_image_carries_the_wrapper_and_arm64_links_only() {
        let files = arm64_files();
        let paths: Vec<&str> = files.iter().map(|file| file.path.as_str()).collect();
        assert!(paths.contains(&"bin/claude"));
        assert!(paths.contains(&"lib/ld-linux-aarch64.so.1"));
        assert!(paths.contains(&"lib/aarch64-linux-gnu"));
        assert!(paths.contains(&"usr/lib/aarch64-linux-gnu"));
        assert!(
            !paths
                .iter()
                .any(|path| path.contains("x86_64") || *path == "lib64")
        );
        assert!(!paths.contains(&"usr/share/applications/claude.desktop"));
    }

    #[test]
    fn the_screen_is_read_through_its_cursor_movements() {
        // As the TUI drew it on 2026-09-30: each word placed by column.
        let drawn = transcript(&[
            "\u{1b}]0;\u{2733} Claude Code\u{7}\u{1b}[H\r\u{1b}[11C\u{1b}[1BClaude\u{1b}[19GCode",
            "\u{1b}[1B\u{25cf}\u{1b}[3Gclaude-code-gate:\u{1b}[21Gbash\u{1b}[26Gon\u{1b}[29GFerrix\
             \u{1b}[36Gsaid\u{1b}[41Gferrix-bash-42\r\u{1b}[5B? for shortcuts\u{1b}[K",
        ]);
        let screen = screen_text(&drawn);
        assert!(screen.contains(&squeeze(REPLY)), "{screen}");
        assert!(screen.contains("ClaudeCode") && screen.contains("?forshortcuts"));
        assert!(!screen.contains("[11C") && !screen.contains("0;"));
    }

    #[test]
    fn the_reachability_check_needs_no_key() {
        let mut asked = request("/api/hello", "");
        asked.line = "HEAD /api/hello HTTP/1.1".to_owned();
        asked.headers.clear();
        assert_eq!(respond(&asked).3, "hello");
    }

    #[test]
    fn a_side_request_gets_a_whole_message() {
        let (_, content_type, body, what) = respond(&request("/v1/messages", "{}"));
        assert_eq!((content_type, what), ("application/json", "side request"));
        assert!(body.contains("\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]"));
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\nd\u{1}"), "\"a\\\"b\\\\c\\nd\\u0001\"");
    }

    #[test]
    fn a_claude_code_that_ran_its_tool_passes() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            VERSION,
            REPLY,
            "\u{1b}[?25h  init     the shell exited with 18",
        ]);
        assert!(judge(Arch::X86_64, &lines, &ran()).is_ok());
    }

    #[test]
    fn a_reply_the_api_never_sent_fails() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            VERSION,
            REPLY,
            "  init     the shell exited with 18",
        ]);
        assert!(judge(Arch::X86_64, &lines, &Seen::default()).is_err());
    }

    #[test]
    fn a_failed_prompt_fails() {
        let lines = transcript(&[
            qemu::SUCCESS_MARKER,
            VERSION,
            "  init     the shell exited with 4",
        ]);
        assert!(judge(Arch::X86_64, &lines, &ran()).is_err());
    }
}
