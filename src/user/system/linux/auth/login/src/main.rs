//! `/bin/login [NAME]`: a person logs in on the console (`docs/AUTH.md`
//! §6.2, P2.3).
//!
//! It asks for a name, then holds the `login` conversation with `authd`,
//! which asks for the password -- or, for a person's account with none, on
//! the console alone, offers to set one (§5.4). `login` reads no credential:
//! it shows `authd`'s prompts and reads the answers from the terminal with
//! its echo off. `authd` takes the `login` service only from root, which is
//! what getty is, so a user running this file is refused there.
//!
//! On ACCEPTED, as root still, it asks the init for the session's scope,
//! `user-<uid>.slice/session-<n>.scope`, `n` counted from 1 at every boot;
//! an init that refuses is said on the console and the login goes on, so
//! nothing yet may rely on the scope (§6.2). Then it becomes the account --
//! its groups from `/etc/group`, its gid and its uid, each checked and every
//! id read back -- and execs its shell as a login shell, in its home, with
//! an environment of its own. Three wrong passwords end it, and getty's
//! restart brings the prompt back.
//!
//! On the console, as root, it first stops the console's last session
//! (`session.rs`), and names its own as the console's once it has one.

mod session;

use std::io::{BufRead as _, Write as _};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitCode};
use std::time::Duration;

use ferrix_auth_account as account;
use ferrix_auth_client::{Connection, Terminal, Verdict, converse};

/// How many passwords one run of `login` takes.
const TRIES: usize = 3;

/// The `PATH` a session starts with.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let named = match arguments.as_slice() {
        [] => None,
        [name] if account::is_a_name(name) => Some(name.clone()),
        _ => {
            say("usage: login [NAME]");
            return ExitCode::from(2);
        }
    };
    // SAFETY: no arguments.
    let console = session::on_console() && unsafe { libc::geteuid() } == 0;
    if console {
        match session::end_console() {
            Ok(Some(unit)) => say(&format!("login: the console's last session ended ({unit})")),
            Ok(None) => {}
            Err(why) => say(&format!(
                "login: the console's last session may live on: {why}"
            )),
        }
    }
    for _ in 0..TRIES {
        let Some(name) = named.clone().or_else(ask_name) else {
            return ExitCode::FAILURE;
        };
        let verdict = Connection::open()
            .and_then(|connection| converse(&connection, "login", &name, &mut Terminal::new()));
        match verdict {
            Ok(Verdict::Accepted { uid, account }) => return start(&account, uid, console),
            Ok(Verdict::Failed {
                retry_after_ms,
                text,
            }) => {
                say(&format!("Login incorrect ({text})"));
                std::thread::sleep(Duration::from_millis(u64::from(retry_after_ms)));
            }
            Ok(Verdict::Unavailable(text)) => {
                say(&format!("login: {text}"));
                return ExitCode::FAILURE;
            }
            Err(error) => {
                say(&format!("login: cannot reach authd: {error}"));
                return ExitCode::FAILURE;
            }
        }
    }
    ExitCode::FAILURE
}

/// `login: `, and the name typed after it; `None` at the end of input.
fn ask_name() -> Option<String> {
    loop {
        let mut out = std::io::stdout();
        let _ = write!(out, "login: ");
        let _ = out.flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).ok()? == 0 {
            return None;
        }
        let name = line.trim();
        if account::is_a_name(name) {
            return Some(name.to_owned());
        }
    }
}

/// What follows `authd`'s ACCEPTED: the scope, the account, its shell.
/// `console` is whether the scope is the console's session.
fn start(name: &str, uid: u32, console: bool) -> ExitCode {
    let Some(account) = account::find(name, "/etc/passwd", "/etc/group") else {
        say(&format!("login: {name} is not in /etc/passwd"));
        return ExitCode::FAILURE;
    };
    // `authd` named the uid; the file must agree, or something changed it
    // in between.
    if account.uid != uid {
        say(&format!(
            "login: {name}'s uid is not the one authd accepted"
        ));
        return ExitCode::FAILURE;
    }
    if !account.may_log_in() {
        say(&format!("login: {name} may not log in"));
        return ExitCode::FAILURE;
    }
    match session::join(account.uid) {
        Ok((slice, unit)) => {
            say(&format!("login: {name} in {slice}/{unit}"));
            if console && let Err(why) = session::remember_console(&slice, &unit) {
                say(&format!(
                    "login: {why}; the next login here will not end this session"
                ));
            }
        }
        Err(why) => say(&format!(
            "login: no session scope for {name}: {why}; logging in without one"
        )),
    }
    if let Err(why) = account::drop_to(&account) {
        say(&format!("login: becoming {name} failed: {why}"));
        return ExitCode::FAILURE;
    }
    let home = if std::env::set_current_dir(&account.home).is_ok() {
        account.home.clone()
    } else {
        let _ = std::env::set_current_dir("/");
        "/".to_owned()
    };
    let term = std::env::var("TERM")
        .ok()
        .filter(|term| account::is_a_term(term));
    let shell_name = account.shell.rsplit('/').next().unwrap_or("sh");
    let mut command = Command::new(&account.shell);
    let _ = command
        .arg0(format!("-{shell_name}"))
        .env_clear()
        .env("HOME", &home)
        .env("USER", &account.name)
        .env("LOGNAME", &account.name)
        .env("SHELL", &account.shell)
        .env("PATH", PATH);
    if let Some(term) = term {
        let _ = command.env("TERM", term);
    }
    let error = command.exec();
    say(&format!("login: {}: {error}", account.shell));
    ExitCode::FAILURE
}

/// Say a line on standard error, which is the terminal.
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}
