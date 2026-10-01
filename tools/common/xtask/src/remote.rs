//! Boot the Ferrix desktop on another machine and watch it on this one.
//!
//! `run-compositor --vnc :0` already serves the screen over VNC, and README.md
//! says how to reach it by hand: push the code over, run the command there,
//! open a tunnel, point a viewer down it. Four steps, three of which are the
//! same every time, and the one that is not -- getting *this* working tree
//! onto that machine -- is the one most easily got wrong, because a boot of
//! yesterday's code looks exactly like a boot of today's.
//!
//! So this holds the four steps and a file holds the answers:
//!
//! ```text
//! cargo xtask remote-desktop
//! ```
//!
//! What it does, in order:
//!
//! 1. Makes a commit of the working tree without touching the index, so what
//!    boots over there is what you are looking at here, uncommitted and all.
//!    `[source] send = "head"` sends the last commit instead.
//! 2. Pushes it to a side ref in a checkout on the remote, making that checkout
//!    the first time, and checks the code out there by hash.
//! 3. Opens one `ssh` that both forwards a local port and runs the boot, so the
//!    tunnel lives exactly as long as the machine does.
//! 4. Waits for the VNC server to answer, then opens a viewer on it.
//!
//! The serial console comes back on this terminal the whole time, because the
//! screen and the console are two different pipes and a boot that fails does so
//! on the console.
//!
//! No machine is named here. The host is an `ssh` destination read from a
//! config file of yours or given as `--host`, which is the rule the rest of
//! `xtask` follows -- no host is a default, and the network is only what an
//! argument asked for. `wallpapers --from <host>:<directory>` is the same
//! shape.

use std::io::{BufRead, BufReader, Read};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::args::Args;
use crate::paths;
use crate::{Error, Result};

/// The ref the code is pushed to over there. Not `refs/heads/...` on purpose:
/// a checkout refuses a push to the branch it has checked out, and a ref
/// outside `refs/heads` is never that branch whatever the checkout is doing.
const REMOTE_REF: &str = "refs/ferrix-desktop/head";

/// Where the remote boot leaves its process id, so a teardown can find it if
/// closing the connection did not take it with it. The third line is the run
/// that started it, from [`run_token`].
const PID_FILE: &str = ".remote-desktop.pid";

/// Config files this looks for when `--config` did not say, best first. The
/// second is under this machine's home directory.
const SEARCH: [&str; 2] = ["remote-desktop.toml", ".config/ferrix/remote.toml"];

/// The variable that names a config file, for a machine with several.
const VAR: &str = "FERRIX_REMOTE";

/// Which commit is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Send {
    /// What you are looking at, uncommitted changes and all.
    WorkingTree,
    /// The last commit, and nothing not committed.
    Head,
}

/// What to open on the tunnel once the screen answers.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Viewer {
    /// Find one of the usual ones on this machine.
    Auto,
    /// Open nothing; the tunnel is the whole of it.
    None,
    /// This command line, with `{address}`, `{host}` and `{port}` filled in.
    Line(Vec<String>),
    /// One of the viewers this knows by name, which it knows how to start
    /// and how it sends the keyboard.
    Known(Known),
}

/// A viewer known by name, because the two do the keyboard differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    /// Sends keys (QEMU's extended key events): the guest's `kb_layout`
    /// makes them characters, as a real keyboard's, and QEMU must be given
    /// no keymap.
    TigerVnc,
    /// Sends characters, which QEMU turns back into keys through a keymap:
    /// the boot is given `--keymap` with the first `--layout`.
    RealVnc,
}

impl Known {
    fn from_word(word: &str) -> Option<Self> {
        match word.to_ascii_lowercase().as_str() {
            "tigervnc" | "tiger" => Some(Known::TigerVnc),
            "realvnc" | "real" => Some(Known::RealVnc),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Known::TigerVnc => "TigerVNC",
            Known::RealVnc => "RealVNC",
        }
    }

    /// Where it is installed, on Windows, and what it is called on a `PATH`
    /// elsewhere.
    const fn places(self) -> (&'static str, &'static str) {
        match self {
            Known::TigerVnc => (r"C:\Program Files\TigerVNC\vncviewer.exe", "vncviewer"),
            Known::RealVnc => (
                r"C:\Program Files\RealVNC\VNC Viewer\vncviewer.exe",
                "realvnc-vnc-viewer",
            ),
        }
    }

    /// The address it is given: `TigerVNC` reads `host:N` as display N, so a
    /// port is `host::port` there.
    fn address(self, port: u16) -> String {
        match self {
            Known::TigerVnc => format!("127.0.0.1::{port}"),
            Known::RealVnc => format!("127.0.0.1:{port}"),
        }
    }

    /// Its command line on this machine, if it is installed.
    fn argv(self, port: u16) -> Option<Vec<String>> {
        let (windows, elsewhere) = self.places();
        let program = if cfg!(windows) {
            Path::new(windows).is_file().then(|| windows.to_owned())
        } else {
            paths::which(elsewhere).map(|found| found.display().to_string())
        }?;
        Some(vec![program, self.address(port)])
    }
}

/// The answers, after the file and the command line have both had their say.
#[derive(Debug, Clone)]
struct Config {
    /// An `ssh` destination, the one key with no default.
    host: String,
    /// The checkout over there, relative to the remote home unless absolute.
    dir: String,
    /// `cargo`, if it is not on a non-interactive shell's `PATH` over there.
    cargo: String,
    /// Anything the remote boot should have in its environment, in order.
    env: Vec<(String, String)>,
    /// The `xtask` command to run over there: `run-compositor`, or `run`.
    command: String,
    /// Its `--arch`.
    arch: String,
    /// Everything else, as it would be written on a command line.
    boot_args: Vec<String>,
    /// The VNC display over there: display n is port 5900 + n.
    display: u32,
    /// The port the tunnel listens on here; `None` is "find a free one".
    local_port: Option<u16>,
    /// What to open on it.
    viewer: Viewer,
    /// How long to wait for the screen, in seconds.
    wait: u64,
    /// Which commit is sent.
    send: Send,
    /// The port over there that `--ssh` forwards to the guest's sshdt, or 0
    /// for no SSH at all.
    ssh_port: u16,
    /// The port the tunnel carries it to here; `None` is "find a free one".
    ssh_local_port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            host: String::new(),
            dir: "ferrix-desktop".to_owned(),
            cargo: "cargo".to_owned(),
            env: Vec::new(),
            command: "run-compositor".to_owned(),
            arch: "x86_64".to_owned(),
            boot_args: Vec::new(),
            display: 0,
            local_port: None,
            viewer: Viewer::Auto,
            wait: 1800,
            send: Send::WorkingTree,
            ssh_port: 22022,
            ssh_local_port: None,
        }
    }
}

/// Boot the desktop over there and show it here.
///
/// # Errors
///
/// No config file and no `--host`, a config file that says something this
/// does not understand, a push or a boot that failed, or a screen that never
/// came up inside `[screen] wait`.
pub(crate) fn remote_desktop(args: &Args) -> Result<()> {
    let (mut config, from) = read_config(args)?;
    apply(&mut config, args)?;
    let directory = remote_dir(&config.dir)?;

    // A boot outlives its connection more often than is comfortable: whatever
    // ended this command before its cleanup ran -- Ctrl-C, a hard kill, a
    // laptop closing -- left QEMU holding that machine's memory and its VNC
    // port, and `ssh`'s own hangup does not always reach it. So the cleanup is
    // a thing you can ask for, rather than only a thing that usually happens.
    if args.stop && !args.print_command {
        println!("stopping any boot of ours on {}:{directory}", config.host);
        teardown(&config.host, &directory, None);
        return Ok(());
    }

    let root = paths::workspace_root();
    let port = pick_port(config.local_port, config.display)?;
    let ssh = pick_ssh_port(&config, port)?;

    if args.print_command {
        if args.stop {
            println!("stop:    {}:{directory}", config.host);
            return Ok(());
        }
        let sha = match config.send {
            Send::Head => git(&root, &["rev-parse", "HEAD"], &[])?,
            Send::WorkingTree => "<working-tree-snapshot>".to_owned(),
        };
        let script = boot_script(
            &config,
            &directory,
            &sha,
            "<run>",
            &forwarded(args, &config.viewer, host_layout()),
        );
        let remote = format!("{}:{directory}", config.host);
        println!("config:  {from}");
        println!("push:    git push {remote} {sha}:{REMOTE_REF}");
        println!(
            "tunnel:  ssh -L {port}:127.0.0.1:{} {}",
            5900_u32.saturating_add(config.display),
            config.host
        );
        if let Some(local) = ssh {
            println!(
                "         -L {local}:127.0.0.1:{}, then: ssh -p {local} root@127.0.0.1",
                config.ssh_port
            );
        }
        println!("boot:    {script}");
        match viewer_argv(&config.viewer, port)? {
            Some(argv) => println!("viewer:  {}", argv.join(" ")),
            None => println!("viewer:  none"),
        }
        return Ok(());
    }

    // Before anything is sent: a missing viewer should be said now, not after
    // a twenty-minute build on the other machine.
    let viewer = viewer_argv(&config.viewer, port)?;
    let (sha, described) = snapshot(config.send, &root)?;
    let run = run_token();
    let script = boot_script(
        &config,
        &directory,
        &sha,
        &run,
        &forwarded(args, &config.viewer, host_layout()),
    );

    println!("config:  {from}");
    println!("sending: {described}");
    println!("      -> {}:{directory}, as {}", config.host, short(&sha));
    prepare(&config.host, &directory)?;
    push(&root, &config.host, &directory, &sha)?;

    println!(
        "screen:  VNC {} over there, 127.0.0.1:{port} here",
        5900_u32.saturating_add(config.display)
    );
    if let Some(local) = ssh {
        println!("ssh:     sshdt in the guest, once it is up: ssh -p {local} root@127.0.0.1");
    }
    println!("         the first boot on a machine builds everything; that is the wait.\n");

    let mut boot = spawn_boot(&config, port, ssh, &script)?;
    let outcome = watch(&config, &mut boot, port, viewer.as_deref());

    // Whatever happened, nothing of ours should still be running over there
    // -- and only ours: when a newer run stopped this boot to take the
    // machine, the pid file already names that run's boot.
    stop_child(&mut boot);
    teardown(&config.host, &directory, Some(&run));
    outcome
}

// --------------------------------------------------------------------------
// The configuration file

/// The config file, read over the defaults, and where it was read from.
fn read_config(args: &Args) -> Result<(Config, String)> {
    let path = find_config(args)?;
    let Some(path) = path else {
        // No file is not an error when the command line said the one thing
        // that has no default; a machine tried once needs no file at all.
        let host = args.host.clone().ok_or_else(|| {
            Error::new(format!(
                "no config file, and no --host. Looked at ${VAR} and:\n  {}\n\
                 Copy tools/common/data/remote-desktop.toml.example to one of those and fill it in, \
                 or give --host <ssh destination>",
                searched().join("\n  ")
            ))
        })?;
        return Ok((
            Config {
                host,
                ..Config::default()
            },
            "none; --host only".to_owned(),
        ));
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|error| Error::new(format!("{}: {error}", path.display())))?;
    let config = parse_config(&text, &path.display().to_string())?;
    Ok((config, path.display().to_string()))
}

/// The files this looks for, spelled for this machine.
fn searched() -> Vec<String> {
    SEARCH
        .iter()
        .map(|name| config_path(name).display().to_string())
        .collect()
}

/// One of the searched names as a path: the second is under the home.
fn config_path(name: &str) -> PathBuf {
    if name.starts_with(".config/") {
        return std::env::home_dir().map_or_else(
            || PathBuf::from(name),
            |home| name.split('/').fold(home, |dir, part| dir.join(part)),
        );
    }
    PathBuf::from(name)
}

/// Where the answers are, in the order this looks for them.
fn find_config(args: &Args) -> Result<Option<PathBuf>> {
    if let Some(explicit) = args.config.as_deref() {
        let path = PathBuf::from(explicit);
        if !path.is_file() {
            return Err(Error::new(format!("no config file at {}", path.display())));
        }
        return Ok(Some(path));
    }
    if let Some(named) = std::env::var_os(VAR) {
        let path = PathBuf::from(named);
        if !path.is_file() {
            return Err(Error::new(format!(
                "{VAR} names {}, which is not a file",
                path.display()
            )));
        }
        return Ok(Some(path));
    }
    Ok(SEARCH
        .iter()
        .map(|name| config_path(name))
        .find(|path| path.is_file()))
}

/// The file over the defaults, refusing a key that means nothing.
///
/// A typo in a key name is otherwise a setting that silently does not apply,
/// which on a tool whose whole job is "the same every time" is the worst kind
/// of bug: everything works and one thing is not what you asked for.
fn parse_config(text: &str, where_from: &str) -> Result<Config> {
    let items = parse_toml(text).map_err(|why| Error::new(format!("{where_from}: {why}")))?;
    let mut config = Config::default();
    let mut host_given = false;
    for (section, key, value) in items {
        let bad = |why: String| Error::new(format!("{where_from}: [{section}] {key}: {why}"));
        match (section.as_str(), key.as_str()) {
            ("remote", "host") => {
                config.host = value.string().map_err(bad)?;
                host_given = !config.host.is_empty();
            }
            ("remote", "dir") => config.dir = value.string().map_err(bad)?,
            ("remote", "cargo") => config.cargo = value.string().map_err(bad)?,
            ("remote.env", _) => {
                let value = value.string().map_err(bad)?;
                config.env.push((key, value));
            }
            ("boot", "command") => config.command = value.string().map_err(bad)?,
            ("boot", "arch") => config.arch = value.string().map_err(bad)?,
            ("boot", "args") => config.boot_args = value.list().map_err(bad)?,
            ("screen", "display") => config.display = value.count().map_err(bad)?,
            ("screen", "local_port") => {
                // `auto` and a number are both answers, and `auto` is the
                // default; a machine that runs a VNC server of its own is the
                // reason the default is not 5900 + display.
                config.local_port = match value {
                    Value::Str(ref word) if word == "auto" => None,
                    Value::Int(_) => {
                        Some(u16::try_from(value.count().map_err(bad)?).map_err(|_| {
                            Error::new(format!("{where_from}: [screen] local_port is not a port"))
                        })?)
                    }
                    _ => return Err(bad("a number, or `auto`".to_owned())),
                };
            }
            ("screen", "viewer") => config.viewer = viewer_of(&value).map_err(bad)?,
            ("screen", "wait") => config.wait = u64::from(value.count().map_err(bad)?),
            ("source", "send") => {
                config.send = match value.string().map_err(bad)?.as_str() {
                    "working-tree" => Send::WorkingTree,
                    "head" => Send::Head,
                    other => {
                        return Err(bad(format!("`working-tree` or `head`, not `{other}`")));
                    }
                };
            }
            ("ssh", "port") => {
                config.ssh_port = u16::try_from(value.count().map_err(bad)?)
                    .map_err(|_| Error::new(format!("{where_from}: [ssh] port is not a port")))?;
            }
            ("ssh", "local_port") => {
                config.ssh_local_port = match value {
                    Value::Str(ref word) if word == "auto" => None,
                    Value::Int(_) => Some(
                        u16::try_from(value.count().map_err(bad)?)
                            .ok()
                            .filter(|port| *port > 0)
                            .ok_or_else(|| {
                                Error::new(format!("{where_from}: [ssh] local_port is not a port"))
                            })?,
                    ),
                    _ => return Err(bad("a number, or `auto`".to_owned())),
                };
            }
            ("remote" | "boot" | "screen" | "source" | "ssh", _) => {
                return Err(Error::new(format!(
                    "{where_from}: [{section}] has no `{key}`. It has: {}",
                    known(&section)
                )));
            }
            _ => {
                return Err(Error::new(format!(
                    "{where_from}: no section named [{section}]. There are: \
                     [remote], [remote.env], [boot], [screen], [source], [ssh]"
                )));
            }
        }
    }
    if !host_given {
        return Err(Error::new(format!(
            "{where_from}: [remote] host is the one thing with no default"
        )));
    }
    Ok(config)
}

/// The keys a section has, for an error that says what was meant instead.
fn known(section: &str) -> &'static str {
    match section {
        "remote" => "host, dir, cargo",
        "boot" => "command, arch, args",
        "screen" => "display, local_port, viewer, wait",
        "ssh" => "port, local_port",
        _ => "send",
    }
}

/// `[screen] viewer`: a word, or a command line as a list.
fn viewer_of(value: &Value) -> std::result::Result<Viewer, String> {
    match value {
        Value::Str(word) if word == "auto" => Ok(Viewer::Auto),
        Value::Str(word) if word == "none" => Ok(Viewer::None),
        Value::Str(word) if Known::from_word(word).is_some() => {
            Ok(Known::from_word(word).map_or(Viewer::Auto, Viewer::Known))
        }
        // A command line written as one string, for somebody who would
        // rather write it that way. A list needs no quoting and is better.
        Value::Str(line) => Ok(Viewer::Line(split_line(line))),
        Value::List(parts) => Ok(Viewer::Line(parts.clone())),
        Value::Int(_) => Err("`auto`, `none`, `tigervnc`, `realvnc`, or a command line".to_owned()),
    }
}

/// What the command line says over the file.
fn apply(config: &mut Config, args: &Args) -> Result<()> {
    if let Some(host) = args.host.clone() {
        config.host = host;
    }
    if let Some(display) = args.vnc.as_deref() {
        config.display = display
            .trim_start_matches(':')
            .parse()
            .map_err(|_| Error::new(format!("--vnc wants a display like `:0`, got `{display}`")))?;
    }
    if let Some(port) = args.local_port {
        config.local_port = Some(port);
    }
    if let Some(send) = args.send.as_deref() {
        config.send = match send {
            "working-tree" => Send::WorkingTree,
            "head" => Send::Head,
            other => {
                return Err(Error::new(format!(
                    "--send is `working-tree` or `head`, not `{other}`"
                )));
            }
        };
    }
    if let Some(word) = args.viewer.as_deref() {
        config.viewer = match word {
            "auto" => Viewer::Auto,
            "none" => Viewer::None,
            other => Viewer::Known(Known::from_word(other).ok_or_else(|| {
                Error::new(format!(
                    "--viewer is `tigervnc`, `realvnc`, `auto` or `none`, not `{other}`"
                ))
            })?),
        };
    }
    if args.no_viewer {
        config.viewer = Viewer::None;
    }
    if config.host.is_empty() {
        return Err(Error::new(
            "no host: [remote] host in the config file, or --host <ssh destination>",
        ));
    }
    if config.host.starts_with('-') || config.host.chars().any(char::is_whitespace) {
        return Err(Error::new("host must be an SSH destination, not options"));
    }
    if config.display > u32::from(u16::MAX) - 5900 {
        return Err(Error::new("VNC display must be between 0 and 59635"));
    }
    if config.local_port == Some(0) {
        return Err(Error::new("local port must be between 1 and 65535"));
    }
    for (name, _) in &config.env {
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(Error::new(format!(
                "invalid environment variable name: {name}"
            )));
        }
    }
    Ok(())
}

// --------------------------------------------------------------------------
// Enough TOML for the answers, and no more

/// A value the config file can hold.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// A quoted string, basic or literal.
    Str(String),
    /// A whole number.
    Int(i64),
    /// An array of strings.
    List(Vec<String>),
}

impl Value {
    /// This as a string, or what it is instead.
    fn string(&self) -> std::result::Result<String, String> {
        match self {
            Value::Str(text) => Ok(text.clone()),
            Value::Int(_) => Err("a string was wanted, and this is a number".to_owned()),
            Value::List(_) => Err("a string was wanted, and this is a list".to_owned()),
        }
    }

    /// This as a list of strings.
    fn list(&self) -> std::result::Result<Vec<String>, String> {
        match self {
            Value::List(parts) => Ok(parts.clone()),
            _ => {
                Err("a list of strings was wanted, as [\"--release\", \"--smp\", \"8\"]".to_owned())
            }
        }
    }

    /// This as a count: a number that is not negative.
    fn count(&self) -> std::result::Result<u32, String> {
        match self {
            Value::Int(number) => {
                u32::try_from(*number).map_err(|_| format!("{number} is not a count"))
            }
            _ => Err("a number was wanted".to_owned()),
        }
    }
}

/// The subset of TOML the answers are written in: sections, `key = value`,
/// strings, numbers and arrays of strings.
///
/// A parser rather than a dependency for the reason `Cargo.toml` gives: this
/// is the program that builds the operating system, and its dependency list
/// should be short enough to read. Anything outside the subset is refused by
/// name rather than ignored, so a file this cannot read never half-applies.
fn parse_toml(text: &str) -> std::result::Result<Vec<(String, String, Value)>, String> {
    let mut items = Vec::new();
    let mut section = String::new();
    let mut lines = text.lines().enumerate().peekable();
    while let Some((number, raw)) = lines.next() {
        let at = number.saturating_add(1);
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
        {
            section = name.trim().to_owned();
            if section.is_empty() {
                return Err(format!("line {at}: a section with no name"));
            }
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {at}: `{line}` is not `key = value`"));
        };
        if section.is_empty() {
            return Err(format!("line {at}: `{key}` is outside every section"));
        }
        // An array may be written over several lines, which is how a long
        // list of boot arguments stays readable.
        let mut text = value.trim().to_owned();
        while text.starts_with('[') && !text.ends_with(']') {
            let Some((_, more)) = lines.next() else {
                return Err(format!("line {at}: a list that is never closed"));
            };
            text.push(' ');
            text.push_str(strip_comment(more).trim());
        }
        let parsed = parse_value(&text).map_err(|why| format!("line {at}: {why}"))?;
        items.push((section.clone(), key.trim().to_owned(), parsed));
    }
    Ok(items)
}

/// The line without a comment, which a `#` outside quotes begins.
fn strip_comment(line: &str) -> &str {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (at, character) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            continue;
        }
        match (quote, character) {
            (None, '#') => return line.split_at(at).0,
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if open == character => quote = None,
            _ => {}
        }
    }
    line
}

/// One value: a string, a number, or an array of strings.
fn parse_value(text: &str) -> std::result::Result<Value, String> {
    if let Some(inside) = text
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    {
        let mut parts = Vec::new();
        for part in split_commas(inside) {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            match parse_value(part)? {
                Value::Str(word) => parts.push(word),
                _ => return Err("a list holds strings".to_owned()),
            }
        }
        return Ok(Value::List(parts));
    }
    if let Some(inside) = text
        .strip_prefix('\'')
        .and_then(|rest| rest.strip_suffix('\''))
    {
        // A literal string is what is written in it, which is why a Windows
        // path in the example is written this way.
        return Ok(Value::Str(inside.to_owned()));
    }
    if let Some(inside) = text
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
    {
        return unescape(inside).map(Value::Str);
    }
    text.parse()
        .map(Value::Int)
        .map_err(|_| format!("`{text}` is not a string, a number or a list"))
}

/// A basic string's contents, with its escapes undone.
fn unescape(inside: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(inside.len());
    let mut characters = inside.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            out.push(character);
            continue;
        }
        match characters.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some(other) => return Err(format!("`\\{other}` is not an escape this reads")),
            None => return Err("a string ending in a backslash".to_owned()),
        }
    }
    Ok(out)
}

/// An array's items, splitting on the commas that are not inside quotes.
fn split_commas(inside: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for character in inside.chars() {
        if escaped {
            escaped = false;
            current.push(character);
            continue;
        }
        if quote == Some('"') && character == '\\' {
            escaped = true;
            current.push(character);
            continue;
        }
        match (quote, character) {
            (None, ',') => {
                parts.push(std::mem::take(&mut current));
                continue;
            }
            (None, '"' | '\'') => quote = Some(character),
            (Some(open), _) if open == character => quote = None,
            _ => {}
        }
        current.push(character);
    }
    parts.push(current);
    parts
}

/// A command line written as one string, split on spaces outside quotes.
fn split_line(line: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for character in line.chars() {
        match (quote, character) {
            (None, ' ' | '\t') => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
                continue;
            }
            (None, '"' | '\'') => {
                quote = Some(character);
                continue;
            }
            (Some(open), _) if open == character => {
                quote = None;
                continue;
            }
            _ => {}
        }
        current.push(character);
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

// --------------------------------------------------------------------------
// What gets sent

/// The commit to boot over there, and a word for what it is.
///
/// `working-tree` is the default because the question being asked is almost
/// always "does my change work", and a change you have not committed is still
/// a change. It is built in an index of its own -- `GIT_INDEX_FILE` -- so the
/// real index is untouched: the alternative, `git stash` or `git add`, is
/// shared with every other worktree of this repository and has cost work here
/// before. Nothing is committed to any branch; the commit is an object that
/// exists to be pushed, and `git gc` collects it once the side ref moves on.
fn snapshot(send: Send, root: &Path) -> Result<(String, String)> {
    let head = git(root, &["rev-parse", "HEAD"], &[])?;
    let subject = git(root, &["log", "-1", "--format=%s"], &[])?;
    if send == Send::Head {
        return Ok((head, format!("HEAD ({subject})")));
    }
    let dirty = git(root, &["status", "--porcelain"], &[])?;
    if dirty.is_empty() {
        return Ok((head, format!("HEAD, working tree clean ({subject})")));
    }
    let scratch = scratch_index()?;
    let index = [("GIT_INDEX_FILE", scratch.display().to_string())];
    let built = (|| -> Result<String> {
        let _ = git(root, &["read-tree", "HEAD"], &index)?;
        let _ = git(root, &["add", "--all"], &index)?;
        git(root, &["write-tree"], &index)
    })();
    // The scratch index is ours and nothing else reads it; a failure to clean
    // it up is not a reason to fail the boot.
    drop(std::fs::remove_file(&scratch));
    let tree = built?;
    let commit = git(
        root,
        &[
            "commit-tree",
            &tree,
            "-p",
            &head,
            "-m",
            "remote-desktop: the working tree as it was",
        ],
        &[],
    )?;
    let differ = dirty.lines().count();
    Ok((
        commit,
        format!("the working tree ({differ} files differ from HEAD)"),
    ))
}

/// A path for the private index, in this machine's temporary directory.
fn scratch_index() -> Result<PathBuf> {
    let name = format!("ferrix-remote-desktop-{}.index", std::process::id());
    let path = std::env::temp_dir().join(name);
    // An index left by an earlier run would be read instead of HEAD.
    drop(std::fs::remove_file(&path));
    Ok(path)
}

/// `git` in this checkout, with an environment this may want to add to.
fn git(root: &Path, args: &[&str], env: &[(&str, String)]) -> Result<String> {
    let mut command = Command::new("git");
    let _ = command.current_dir(root).args(args).stdin(Stdio::null());
    for (name, value) in env {
        let _ = command.env(name, value);
    }
    let output = command.output().map_err(|error| {
        Error::new(format!("`git {}` would not start: {error}", args.join(" ")))
    })?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(Error::new(format!(
            "`git {}` failed\n  {}",
            args.join(" "),
            detail.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// The first twelve of a hash, which is what a person reads.
fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

// --------------------------------------------------------------------------
// The remote side

/// The directory over there, as a remote shell will read it.
///
/// `~` is not expanded by this machine and must not be quoted into a literal
/// over there, so a path under the remote home is written relative and the
/// remote shell's own working directory -- the home -- does the rest.
fn remote_dir(given: &str) -> Result<String> {
    let path = given.trim().strip_prefix("~/").unwrap_or(given.trim());
    if matches!(path, "~" | "." | "/") {
        return Err(Error::new(
            "[remote] dir cannot be the home directory itself",
        ));
    }
    if path.is_empty() {
        return Err(Error::new("[remote] dir is empty"));
    }
    Ok(path.to_owned())
}

/// `ssh` with the options this needs and nothing about any host.
///
/// Everything else -- the user, the key, the `ProxyJump` two hops away -- is
/// `~/.ssh/config`'s business, which is where a person already keeps it.
/// `BatchMode` is *not* set, unlike `wallpapers`: this is a command somebody
/// is sitting in front of, and a passphrase prompt is an answer, not a hang.
fn ssh(host: &str) -> Command {
    let mut command = Command::new("ssh");
    let _ = command.args(["-o", "BatchMode=no", "-o", "ServerAliveInterval=30", host]);
    command
}

/// `word` as one word of a POSIX shell's command line, whatever is in it.
fn quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// Make the checkout over there, once, and say nothing when it is there.
fn prepare(host: &str, directory: &str) -> Result<()> {
    let script = [
        "set -e".to_owned(),
        format!("mkdir -p {}", quoted(directory)),
        format!("cd {}", quoted(directory)),
        "test -d .git || git init --quiet .".to_owned(),
    ]
    .join("; ");
    let mut command = ssh(host);
    let _ = command.arg(script).stdin(Stdio::null());
    let output = command
        .output()
        .map_err(|error| Error::new(format!("`ssh {host}` would not start: {error}")))?;
    if !output.status.success() {
        return Err(Error::new(format!(
            "could not make {host}:{directory}\n  {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

/// Push the commit to the side ref over there.
fn push(root: &Path, host: &str, directory: &str, sha: &str) -> Result<()> {
    let remote = format!("{host}:{directory}");
    let refspec = format!("{sha}:{REMOTE_REF}");
    let _ = git(
        root,
        &["push", "--quiet", "--force", &remote, &refspec],
        &[],
    )?;
    Ok(())
}

/// This machine's keyboard layout and variant, for [`forwarded`].
fn host_layout() -> Option<(String, Option<String>)> {
    crate::keyboard::host().map(|(layout, variant, _)| (layout, variant))
}

/// The flags on this command line that are the boot's rather than this
/// command's -- the keyboard's layout and variant -- then everything after
/// `--`. They are parsed with the rest, since there is one parser, and a flag
/// this command reads but does not pass on is one that silently does
/// nothing: `--layout de` once booted a guest typing in `us`.
///
/// `--keymap` too, and for `RealVNC` one follows from `--layout` when none was
/// given: it sends characters, and QEMU turns a German `~` back into `AltGr`
/// and `+` only through the German keymap.
///
/// Given neither `--layout` nor `--variant`, the layout is `host`'s -- *this*
/// machine's, from [`host_layout`] -- passed on as if it had been typed: the
/// keyboard is the one in front of the viewer, and the machine over there,
/// which would otherwise read its own, may have another.
fn forwarded(args: &Args, viewer: &Viewer, host: Option<(String, Option<String>)>) -> Vec<String> {
    let (layout, variant) = if args.layout.is_none() && args.variant.is_none() {
        host.map_or((None, None), |(layout, variant)| (Some(layout), variant))
    } else {
        (args.layout.clone(), args.variant.clone())
    };
    let keymap = args.keymap.clone().or_else(|| {
        (*viewer == Viewer::Known(Known::RealVnc))
            .then(|| layout.clone())
            .flatten()
    });
    let mut extra = Vec::new();
    for (flag, value) in [
        ("--layout", &layout),
        ("--variant", &variant),
        ("--keymap", &keymap),
    ] {
        if let Some(value) = value {
            extra.push(flag.to_owned());
            extra.push(value.clone());
        }
    }
    // Chrome on the desktop, which only the boot over there can give it.
    if args.everything {
        extra.push("--everything".to_owned());
    } else if args.chrome {
        extra.push("--chrome".to_owned());
    }
    extra.extend(args.passthrough.iter().cloned());
    extra
}

/// The one command the boot `ssh` runs over there.
///
/// It is one string and not a script on stdin, deliberately: QEMU's serial
/// console reads that connection's stdin, and a script fed that way is eaten
/// by the guest a line at a time. The caller closes stdin as well.
///
/// `extra` is [`forwarded`]: the flags meant for the boot, and then whatever
/// came after `--`. `run` goes into the pid file, so this run's teardown can
/// tell its own boot from a newer run's.
fn boot_script(config: &Config, directory: &str, sha: &str, run: &str, extra: &[String]) -> String {
    let mut argv = vec![
        config.cargo.clone(),
        "xtask".to_owned(),
        config.command.clone(),
        "--arch".to_owned(),
        config.arch.clone(),
        "--vnc".to_owned(),
        format!(":{}", config.display),
    ];
    // `run-compositor` is the command that starts sshdt; `run`'s shell is a
    // serial console with nothing to start it from.
    if config.ssh_port != 0 && config.command == "run-compositor" {
        argv.extend(["--ssh".to_owned(), config.ssh_port.to_string()]);
    }
    argv.extend(config.boot_args.iter().cloned());
    argv.extend(extra.iter().cloned());
    let line = argv
        .iter()
        .map(|word| quoted(word))
        .collect::<Vec<_>>()
        .join(" ");

    let mut steps = vec![
        "set -e".to_owned(),
        // rustup and QEMU are both usually installed under the home, and a
        // non-interactive `ssh` reads none of the files that would say so.
        "export PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:/usr/local/bin:$PATH\"".to_owned(),
    ];
    steps.extend(
        config
            .env
            .iter()
            .map(|(name, value)| format!("export {name}={}", quoted(value))),
    );
    steps.push(format!("cd {}", quoted(directory)));
    // A boot still running from an earlier run -- one whose connection did
    // not take it along -- holds the VNC port. Left alone, this boot's QEMU
    // fails to bind it, the viewer is shown the old screen instead, and the
    // pid file below loses the only record of the old boot, so `--stop`
    // cannot find it again. So it goes first, before the checkout changes the
    // files under it.
    steps.push(stop_recorded(PID_FILE, None));
    // This checkout is the cache owned by remote-desktop. A previous boot or
    // an interrupted/manual edit may have left tracked files dirty; those
    // files must not prevent the next requested snapshot from starting.
    steps.push(format!(
        "git checkout --quiet --force --detach {}",
        quoted(sha)
    ));
    steps.push("echo \"remote: booting $(git rev-parse --short HEAD) in $PWD\"".to_owned());
    // For a teardown that has to reach past a connection which did not take
    // the boot with it when it closed.
    steps.push(format!(
        "printf \"%s\\n%s\\n%s\\n\" \"$$\" \"$(ps -o pgid= -p $$ | tr -d ' ')\" {} > {PID_FILE}",
        quoted(run)
    ));
    steps.push(format!("exec {line}"));
    steps.join("; ")
}

/// A shell step stopping the boot the pid file at `pid_path` names, if there
/// is one: its process group, then the process itself, then a wait of up to
/// ten seconds for both to be gone, so a boot started next finds the VNC port
/// free. Safe under `set -e`, and it never ends the script it is part of.
///
/// With `run`, only a boot that run started: a run cleaning up after itself
/// must not stop the boot of a newer run that has since taken the machine.
fn stop_recorded(pid_path: &str, run: Option<&str>) -> String {
    let path = quoted(pid_path);
    let ours = run.map_or_else(
        || "true".to_owned(),
        |run| format!("test \"$(sed -n 3p {path})\" = {}", quoted(run)),
    );
    [
        format!("if test -f {path} && {ours}; then pid=$(sed -n 1p {path})"),
        format!("pgid=$(sed -n 2p {path})"),
        "if test -n \"$pgid\"; then kill -TERM -\"$pgid\" 2>/dev/null || true; fi".to_owned(),
        "if test -n \"$pid\"; then kill -TERM \"$pid\" 2>/dev/null || true; fi".to_owned(),
        // An empty id is not waited for: `kill -0 0` would ask about this
        // shell's own group, which is always there.
        "for _ in 1 2 3 4 5 6 7 8 9 10; do \
         if test -n \"$pgid\" && kill -0 -\"$pgid\" 2>/dev/null; then :; \
         elif test -n \"$pid\" && kill -0 \"$pid\" 2>/dev/null; then :; \
         else break; fi; sleep 1; done"
            .to_owned(),
        format!("rm -f {path}; fi"),
    ]
    .join("; ")
}

/// Best effort: make sure nothing of ours is still running over there.
///
/// Closing the connection sends the boot a `SIGHUP` and that is usually the
/// end of it. Usually is not always, and a QEMU nobody is watching holds a
/// machine's memory and its VNC port against the next run, so this asks.
///
/// `run` is [`stop_recorded`]'s: `None` stops whatever boot is recorded.
fn teardown(host: &str, directory: &str, run: Option<&str>) {
    let script = stop_recorded(&format!("{directory}/{PID_FILE}"), run);
    let mut command = ssh(host);
    let _ = command
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    drop(command.status());
}

/// What names this run in the pid file over there: this process and the
/// moment it asked, which two runs on one machine do not share.
fn run_token() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}-{}", std::process::id(), since.as_nanos())
}

// --------------------------------------------------------------------------
// This side: the port, the wait, the viewer

/// Which local port the tunnel listens on.
///
/// `auto` because the obvious choice, 5900 + display, is also what a VNC
/// server already running on *this* machine would have taken, and the failure
/// that causes -- a viewer connecting to the wrong screen -- looks like the
/// remote boot having gone wrong.
fn pick_port(asked: Option<u16>, display: u32) -> Result<u16> {
    if let Some(port) = asked {
        return Ok(port);
    }
    let first = u16::try_from(5900_u32.saturating_add(display))
        .map_err(|_| Error::new("that display is not a port near 5900"))?;
    for candidate in first..=first.saturating_add(63) {
        if std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return Ok(candidate);
        }
    }
    Err(Error::new(
        "no free local port near 5900; set [screen] local_port or --local-port",
    ))
}

/// Which local port the tunnel carries the guest's SSH to, or `None` when
/// this boot has none: `[ssh] port = 0`, or a command other than
/// `run-compositor`, which is the one that starts sshdt.
///
/// `auto` starts at the remote's own port, so the usual answer is the same
/// number on both machines, and moves up past anything already listening
/// here -- including the VNC tunnel's own port.
fn pick_ssh_port(config: &Config, vnc: u16) -> Result<Option<u16>> {
    if config.ssh_port == 0 || config.command != "run-compositor" {
        return Ok(None);
    }
    if let Some(port) = config.ssh_local_port {
        return Ok(Some(port));
    }
    let first = config.ssh_port;
    for candidate in first..=first.saturating_add(63) {
        if candidate != vnc && std::net::TcpListener::bind(("127.0.0.1", candidate)).is_ok() {
            return Ok(Some(candidate));
        }
    }
    Err(Error::new(format!(
        "no free local port near {first} for SSH; set [ssh] local_port, or [ssh] port = 0"
    )))
}

/// Whether a VNC server is on the other end of the tunnel yet.
///
/// The test is the protocol's own greeting, not a connection: `ssh` accepts on
/// the forwarded port from the moment it starts, long before anything over
/// there is listening, so a connection that succeeds proves only that `ssh` is
/// running. `RFB 003.00x` proves QEMU is.
fn answering(port: u16) -> bool {
    let Ok(mut link) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    drop(link.set_read_timeout(Some(Duration::from_secs(2))));
    let mut greeting = [0_u8; 12];
    let mut filled = 0_usize;
    while filled < greeting.len() {
        match link.read(greeting.get_mut(filled..).unwrap_or_default()) {
            Ok(0) | Err(_) => return false,
            Ok(read) => filled = filled.saturating_add(read),
        }
    }
    greeting.starts_with(b"RFB ")
}

/// The viewer to open, as a command line, or `None` for none.
fn viewer_argv(viewer: &Viewer, port: u16) -> Result<Option<Vec<String>>> {
    let address = format!("127.0.0.1:{port}");
    match viewer {
        Viewer::None => Ok(None),
        Viewer::Line(parts) => Ok(Some(
            parts
                .iter()
                .map(|part| fill(part, &address, port))
                .collect(),
        )),
        Viewer::Known(known) => known.argv(port).map(Some).ok_or_else(|| {
            let (windows, elsewhere) = known.places();
            let place = if cfg!(windows) { windows } else { elsewhere };
            Error::new(format!(
                "{} is not installed here ({place}); install it, or pick another with --viewer",
                known.name()
            ))
        }),
        Viewer::Auto => found_viewer(&address).map(Some).ok_or_else(|| {
            Error::new(format!(
                "no VNC viewer found. Install one (RealVNC, TigerVNC, Remmina), or set\n  \
                 [screen] viewer to its command line, or to `none` and connect by hand to\n  \
                 {address}"
            ))
        }),
    }
}

/// `{address}`, `{host}` and `{port}` in a viewer's command line.
fn fill(part: &str, address: &str, port: u16) -> String {
    part.replace("{address}", address)
        .replace("{host}", "127.0.0.1")
        .replace("{port}", &port.to_string())
}

/// A viewer this machine has, if it has one of the usual ones.
fn found_viewer(address: &str) -> Option<Vec<String>> {
    if cfg!(windows) {
        for candidate in [
            // TigerVNC first: it sends keys, which the guest's layout reads
            // as a real keyboard's, where RealVNC sends characters.
            r"C:\Program Files\TigerVNC\vncviewer.exe",
            r"C:\Program Files\RealVNC\VNC Viewer\vncviewer.exe",
            r"C:\Program Files\uvnc bvba\UltraVNC\vncviewer.exe",
        ] {
            if Path::new(candidate).is_file() {
                return Some(vec![candidate.to_owned(), address.to_owned()]);
            }
        }
    }
    for name in ["vncviewer", "gvncviewer", "vinagre"] {
        if let Some(found) = paths::which(name) {
            return Some(vec![found.display().to_string(), address.to_owned()]);
        }
    }
    if cfg!(target_os = "macos") {
        return Some(vec!["open".to_owned(), format!("vnc://{address}")]);
    }
    paths::which("remmina").map(|found| {
        vec![
            found.display().to_string(),
            "-c".to_owned(),
            format!("vnc://{address}"),
        ]
    })
}

// --------------------------------------------------------------------------
// The boot, and watching it

/// One `ssh` carrying both the forward and the boot, so the tunnel lives
/// exactly as long as the machine does and neither can outlive the other.
///
/// `ssh` is the local port the guest's SSH comes to, from [`pick_ssh_port`],
/// carried by the same connection for the same reason.
fn spawn_boot(config: &Config, port: u16, ssh: Option<u16>, script: &str) -> Result<Child> {
    let mut command = Command::new("ssh");
    let _ = command
        .args(["-o", "BatchMode=no", "-o", "ServerAliveInterval=30"])
        .args(["-o", "ExitOnForwardFailure=yes"])
        .args([
            "-L",
            &format!(
                "{port}:127.0.0.1:{}",
                5900_u32.saturating_add(config.display)
            ),
        ]);
    if let Some(local) = ssh {
        let _ = command.args(["-L", &format!("{local}:127.0.0.1:{}", config.ssh_port)]);
    }
    let _ = command
        .arg(&config.host)
        .arg(script)
        // The guest's console reads this connection's stdin; nothing here
        // should be typing into it.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
        .spawn()
        .map_err(|error| Error::new(format!("`ssh {}` would not start: {error}", config.host)))
}

/// Put the boot's output on this terminal as it arrives, on both its pipes.
fn pump(boot: &mut Child) {
    for stream in [
        boot.stdout
            .take()
            .map(|out| Box::new(out) as Box<dyn Read + core::marker::Send>),
        boot.stderr
            .take()
            .map(|err| Box::new(err) as Box<dyn Read + core::marker::Send>),
    ]
    .into_iter()
    .flatten()
    {
        drop(std::thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines().map_while(std::result::Result::ok) {
                println!("{line}");
            }
        }));
    }
}

/// Wait for the screen, open the viewer, and hold until one of them ends.
fn watch(config: &Config, boot: &mut Child, port: u16, viewer: Option<&[String]>) -> Result<()> {
    pump(boot);

    let deadline = Instant::now()
        .checked_add(Duration::from_secs(config.wait))
        .ok_or_else(|| Error::new("[screen] wait is longer than this machine can count"))?;
    loop {
        if let Some(status) = boot.try_wait()? {
            return Err(Error::new(format!(
                "the remote boot ended before the screen came up ({status}). Its output is above."
            )));
        }
        if answering(port) {
            break;
        }
        if Instant::now() >= deadline {
            return Err(Error::new(format!(
                "no VNC server after {}s. The boot is still running; its output is above.",
                config.wait
            )));
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let Some(argv) = viewer else {
        println!(
            "\nscreen: up. Connect a viewer to 127.0.0.1:{port}. After Ctrl-C, use --stop to clean up the remote boot.\n"
        );
        let status = boot.wait()?;
        if !status.success() {
            return Err(Error::new(format!("remote boot failed ({status})")));
        }
        return Ok(());
    };
    println!("\nscreen: up. Opening {}\n", argv.join(" "));
    let mut shown = spawn_viewer(argv)?;

    // Whichever ends first ends the other: closing the window is how a person
    // says they are done, and the boot ending is how the machine says the
    // same thing.
    let ended = loop {
        if let Some(status) = shown.try_wait()? {
            if !status.success() {
                return Err(Error::new(format!("VNC viewer failed ({status})")));
            }
            break "\nviewer closed; stopping the boot.";
        }
        if let Some(status) = boot.try_wait()? {
            if !status.success() {
                stop_child(&mut shown);
                return Err(Error::new(format!("remote boot failed ({status})")));
            }
            break "\nthe boot ended; closing the viewer.";
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    println!("{ended}");
    stop_child(&mut shown);
    Ok(())
}

/// Open the viewer, saying which program it was if it will not start.
fn spawn_viewer(argv: &[String]) -> Result<Child> {
    let (program, rest) = argv
        .split_first()
        .ok_or_else(|| Error::new("[screen] viewer is an empty command line"))?;
    Command::new(program)
        .args(rest)
        .spawn()
        .map_err(|error| Error::new(format!("`{program}` would not start: {error}")))
}

/// End a child of ours, if it is still running.
fn stop_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(None)) {
        drop(child.kill());
        drop(child.wait());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
# A comment, and a blank line.
[remote]
host = "nowhere"
dir = "~/desktop"

[remote.env]
FERRIX_QEMU = "/usr/bin"

[boot]
command = "run-compositor"
arch = "x86_64"
args = ["--release", "--size", "1600x900"]

[screen]
display = 2
local_port = "auto"
viewer = ['C:\viewers\vncviewer.exe', '{address}']
wait = 60

[source]
send = "head"
"#;

    fn example() -> Config {
        parse_config(EXAMPLE, "test.toml").expect("the example parses")
    }

    #[test]
    fn reads_every_kind_of_answer() {
        let config = example();
        assert_eq!(config.host, "nowhere");
        assert_eq!(config.dir, "~/desktop");
        assert_eq!(
            config.env,
            vec![("FERRIX_QEMU".to_owned(), "/usr/bin".to_owned())]
        );
        assert_eq!(config.boot_args, vec!["--release", "--size", "1600x900"]);
        assert_eq!(config.display, 2);
        assert_eq!(config.local_port, None, "`auto` is not a port");
        assert_eq!(config.wait, 60);
        assert_eq!(config.send, Send::Head);
        assert_eq!(
            config.viewer,
            Viewer::Line(vec![
                r"C:\viewers\vncviewer.exe".to_owned(),
                "{address}".to_owned()
            ]),
            "a literal string keeps its backslashes"
        );
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let config = parse_config("[remote]\nhost = \"nowhere\"\n", "test.toml").unwrap();
        assert_eq!(config.dir, "ferrix-desktop");
        assert_eq!(config.cargo, "cargo");
        assert_eq!(config.command, "run-compositor");
        assert_eq!(config.arch, "x86_64");
        assert_eq!(config.display, 0);
        assert_eq!(config.wait, 1800);
        assert_eq!(config.send, Send::WorkingTree);
        assert_eq!(config.viewer, Viewer::Auto);
    }

    #[test]
    fn a_typo_is_refused_rather_than_ignored() {
        // The whole reason this reads the file itself rather than taking
        // whatever it finds: a key that means nothing is a setting that
        // silently does not apply.
        let wrong_key = parse_config("[screen]\nviewr = \"none\"\n", "test.toml");
        assert!(
            wrong_key.is_err_and(|error| error.to_string().contains("viewr")),
            "an unknown key is named"
        );
        let wrong_section = parse_config("[sceen]\nviewer = \"none\"\n", "test.toml");
        assert!(
            wrong_section.is_err_and(|error| error.to_string().contains("[sceen]")),
            "an unknown section is named"
        );
    }

    #[test]
    fn the_host_is_the_one_thing_with_no_default() {
        assert!(parse_config("[boot]\narch = \"aarch64\"\n", "test.toml").is_err());
    }

    #[test]
    fn send_takes_only_its_two_words() {
        assert!(parse_config("[remote]\nhost = \"a\"\n[source]\nsend = \"tree\"\n", "t").is_err());
    }

    #[test]
    fn a_list_may_span_lines() {
        let config = parse_config(
            "[remote]\nhost = \"a\"\n[boot]\nargs = [\n  \"--gl\",\n  \"--smp\", \"8\",\n]\n",
            "t",
        )
        .unwrap();
        assert_eq!(config.boot_args, vec!["--gl", "--smp", "8"]);
    }

    #[test]
    fn a_comment_does_not_cut_a_value() {
        let config =
            parse_config("[remote]\nhost = \"a#b\"  # not a comment, that\n", "t").unwrap();
        assert_eq!(config.host, "a#b");
    }

    #[test]
    fn a_home_relative_directory_is_written_relative() {
        assert_eq!(remote_dir("~/ferrix-desktop").unwrap(), "ferrix-desktop");
        assert_eq!(remote_dir("/srv/ferrix").unwrap(), "/srv/ferrix");
        assert!(
            remote_dir("~").is_err(),
            "the home itself is not a checkout"
        );
    }

    #[test]
    fn the_keyboard_flags_reach_the_boot() {
        let args = Args::parse(
            ["remote-desktop", "--layout", "de", "--", "--smp", "2"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            forwarded(&args, &Viewer::Known(Known::TigerVnc), None),
            ["--layout", "de", "--smp", "2"],
            "a viewer that sends keys gets no keymap"
        );
        assert_eq!(
            forwarded(&args, &Viewer::Known(Known::RealVnc), None),
            ["--layout", "de", "--keymap", "de", "--smp", "2"],
            "a viewer that sends characters gets the layout's keymap"
        );
        let args = Args::parse(
            ["remote-desktop", "--chrome"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            forwarded(&args, &Viewer::Known(Known::TigerVnc), None),
            ["--chrome"],
            "--chrome reaches the boot"
        );
        // Nothing said: the layout of the keyboard in front of the viewer.
        let german = Some(("de".to_owned(), Some("nodeadkeys".to_owned())));
        assert_eq!(
            forwarded(&args, &Viewer::Known(Known::RealVnc), german.clone()),
            [
                "--layout",
                "de",
                "--variant",
                "nodeadkeys",
                "--keymap",
                "de",
                "--chrome"
            ],
            "this machine's layout reaches the boot when none was given"
        );
        // Something said: that, and not this machine's.
        let args = Args::parse(
            ["remote-desktop", "--layout", "us"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert_eq!(
            forwarded(&args, &Viewer::Known(Known::TigerVnc), german),
            ["--layout", "us"],
        );
    }

    #[test]
    fn a_viewer_is_chosen_by_name_and_the_flag_wins() {
        let words = |word: &str| viewer_of(&Value::Str(word.to_owned())).unwrap();
        assert_eq!(words("tigervnc"), Viewer::Known(Known::TigerVnc));
        assert_eq!(words("RealVNC"), Viewer::Known(Known::RealVnc));
        assert_eq!(Known::TigerVnc.address(5901), "127.0.0.1::5901");
        assert_eq!(Known::RealVnc.address(5901), "127.0.0.1:5901");

        let mut config = example();
        config.viewer = Viewer::Known(Known::TigerVnc);
        let args = Args::parse(
            ["remote-desktop", "--viewer", "realvnc"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        apply(&mut config, &args).unwrap();
        assert_eq!(config.viewer, Viewer::Known(Known::RealVnc));

        let args = Args::parse(
            ["remote-desktop", "--viewer", "ultra"]
                .into_iter()
                .map(str::to_owned),
        )
        .unwrap();
        assert!(apply(&mut config, &args).is_err());
    }

    #[test]
    fn a_desktop_boot_starts_sshdt_unless_told_not_to() {
        let config = parse_config("[remote]\nhost = \"nowhere\"\n", "test.toml").unwrap();
        assert_eq!(config.ssh_port, 22022, "SSH is on by default, on 22022");
        let script = boot_script(&config, "desktop", "abc123", "run1", &[]);
        assert!(
            script.contains("'--ssh' '22022'"),
            "run-compositor is asked for sshdt: {script}"
        );

        let off = parse_config(
            "[remote]\nhost = \"nowhere\"\n[ssh]\nport = 0\n",
            "test.toml",
        )
        .unwrap();
        assert!(!boot_script(&off, "desktop", "abc123", "run1", &[]).contains("--ssh"));
        assert_eq!(
            pick_ssh_port(&off, 5900).unwrap(),
            None,
            "and no tunnel for it"
        );

        let run = parse_config(
            "[remote]\nhost = \"nowhere\"\n[boot]\ncommand = \"run\"\n",
            "test.toml",
        )
        .unwrap();
        assert!(
            !boot_script(&run, "desktop", "abc123", "run1", &[]).contains("--ssh"),
            "`run` has nothing to start sshdt from"
        );

        let fixed = parse_config(
            "[remote]\nhost = \"nowhere\"\n[ssh]\nport = 2300\nlocal_port = 4022\n",
            "test.toml",
        )
        .unwrap();
        assert_eq!(pick_ssh_port(&fixed, 5900).unwrap(), Some(4022));
        assert!(parse_config("[remote]\nhost = \"x\"\n[ssh]\nlocal_port = 0\n", "t").is_err());
        assert!(parse_config("[remote]\nhost = \"x\"\n[ssh]\nport = 70000\n", "t").is_err());
    }

    #[test]
    fn the_boot_command_carries_the_display_and_the_extras() {
        let config = example();
        let script = boot_script(
            &config,
            "desktop",
            "abc123",
            "run1",
            &["--smp".to_owned(), "8".to_owned()],
        );
        assert!(script.contains("cd 'desktop'"));
        assert!(script.contains("git checkout --quiet --force --detach 'abc123'"));
        assert!(script.contains("export FERRIX_QEMU='/usr/bin'"));
        assert!(
            script.contains("'--vnc' ':2'"),
            "the display the tunnel forwards is the display served: {script}"
        );
        assert!(script.contains("'--smp' '8'"), "extras come last: {script}");
        let (stop, checkout, record) = (
            script.find("kill -TERM").unwrap(),
            script.find("git checkout").unwrap(),
            script.find(&format!("> {PID_FILE}")).unwrap(),
        );
        assert!(
            stop < checkout && checkout < record,
            "an earlier boot is stopped before its record is overwritten: {script}"
        );
        assert!(
            script.ends_with("'8'"),
            "the boot is exec'd last, so the connection is the machine"
        );
    }

    #[test]
    fn a_run_cleans_up_only_its_own_boot() {
        let script = boot_script(&example(), "desktop", "abc123", "run1", &[]);
        assert!(
            script.contains(&format!("'run1' > {PID_FILE}")),
            "the boot records the run that started it: {script}"
        );
        assert!(
            stop_recorded(PID_FILE, Some("run1"))
                .contains("test \"$(sed -n 3p '.remote-desktop.pid')\" = 'run1'"),
            "a run's teardown stops a boot only when the record names that run"
        );
        assert!(
            !stop_recorded(PID_FILE, None).contains("sed -n 3p"),
            "a new boot, and --stop, stop whatever is recorded"
        );
    }

    #[test]
    fn a_word_with_a_quote_in_it_survives_the_shell() {
        assert_eq!(quoted("it's"), r"'it'\''s'");
    }

    #[test]
    fn the_command_line_says_what_the_file_said() {
        let mut config = example();
        let args = Args {
            host: Some("elsewhere".to_owned()),
            vnc: Some(":7".to_owned()),
            send: Some("working-tree".to_owned()),
            no_viewer: true,
            ..Args::default()
        };
        apply(&mut config, &args).unwrap();
        assert_eq!(config.host, "elsewhere");
        assert_eq!(config.display, 7);
        assert_eq!(config.send, Send::WorkingTree);
        assert_eq!(config.viewer, Viewer::None);
    }

    #[test]
    fn a_viewer_line_is_filled_in() {
        let viewer = Viewer::Line(vec![
            "v".to_owned(),
            "{address}".to_owned(),
            "{port}".to_owned(),
        ]);
        assert_eq!(
            viewer_argv(&viewer, 5901).unwrap().unwrap(),
            vec!["v", "127.0.0.1:5901", "5901"]
        );
        assert_eq!(viewer_argv(&Viewer::None, 5901).unwrap(), None);
    }

    #[test]
    fn a_viewer_written_as_one_string_is_split_on_its_quotes() {
        assert_eq!(
            split_line("\"C:\\Program Files\\v.exe\" {address}"),
            vec![r"C:\Program Files\v.exe", "{address}"]
        );
    }

    #[test]
    fn a_port_asked_for_is_the_port_used() {
        assert_eq!(pick_port(Some(5999), 0).unwrap(), 5999);
    }

    #[test]
    fn escaped_quotes_preserve_comments_and_array_commas() {
        let config = parse_config(
            r##"[remote]
host = "host"
[boot]
args = ["a\"#b,c", "next"] # comment
"##,
            "test.toml",
        )
        .unwrap();
        assert_eq!(config.boot_args, vec!["a\"#b,c", "next"]);
    }

    #[test]
    fn invalid_ports_and_shell_names_are_rejected() {
        let mut config = example();
        config.display = 59636;
        assert!(apply(&mut config, &Args::default()).is_err());
        config.display = 59635;
        assert!(apply(&mut config, &Args::default()).is_ok());
        config.local_port = Some(0);
        assert!(apply(&mut config, &Args::default()).is_err());
        config.local_port = None;
        config.host = "-oProxyCommand=bad".to_owned();
        assert!(apply(&mut config, &Args::default()).is_err());
        config.host = "host".to_owned();
        config.env.push(("BAD;name".to_owned(), "value".to_owned()));
        assert!(apply(&mut config, &Args::default()).is_err());
        assert!(remote_dir("~/").is_err());
    }
}
