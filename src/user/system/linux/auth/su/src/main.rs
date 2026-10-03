//! `/bin/su [-] [-l] [-s SHELL] [-c COMMAND] [USER]`: become another user,
//! root by default (`docs/AUTH.md` §4.2, §6.2, decision 5).
//!
//! Installed set-uid root. Run by root, it asks nothing, as every `su`. Run
//! by anyone else, the target must be root, and the person proves who they
//! are with their *own* password to `authd`'s `su` service, which takes it
//! only from a member of `wheel` (`TargetGroup=wheel`), asking a non-member
//! nothing at all.
//!
//! How `authd` knows who is asking is the kernel's word, not this
//! program's: `su` sets its effective uid back to the person's for the
//! `connect` alone, so `SO_PEERCRED`, which is fixed at connect, names them,
//! and takes root back at once. It talks only to the compiled-in socket and
//! only to a server whose listening uid is the init's or `authd`'s (the
//! certification consultant's U1). Its environment is cleared before
//! anything reads it (U2). The password is read from standard input when
//! that is a terminal, and nowhere else (U5). Root is kept only on an ACCEPTED for the
//! caller's own uid and account (U3); anything else ends `su` without it.

mod args;

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitCode};

use ferrix_auth_account as account;
use ferrix_auth_client::{Connection, Terminal, Verdict, converse};

/// The listening uids `su` will talk to: the init's, which owns a socket it
/// activates, and `authd`'s own account's.
const AUTHD_UIDS: [u32; 2] = [0, 90];

/// The `PATH` the new shell starts with.
const PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin";

fn main() -> ExitCode {
    // U2: before anything reads the environment, it is taken; only `TERM`
    // is kept, and only if it is a plain name.
    let term = std::env::var("TERM")
        .ok()
        .filter(|term| account::is_a_term(term));
    clear_environment();
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(asked) = args::parse(&arguments) else {
        say("usage: su [-] [-l] [-s SHELL] [-c COMMAND] [USER]");
        return ExitCode::from(2);
    };
    match run(&asked, term) {
        Ok(never) => match never {},
        Err(why) => {
            say(&format!("su: {why}"));
            ExitCode::FAILURE
        }
    }
}

/// What can never be made: `run` returns only by failing.
enum Never {}

fn run(asked: &args::Asked, term: Option<String>) -> Result<Never, String> {
    let passwd = std::fs::read_to_string("/etc/passwd").map_err(|e| format!("/etc/passwd: {e}"))?;
    let target = account::find(&asked.user, "/etc/passwd", "/etc/group")
        .ok_or_else(|| format!("there is no account called {}", asked.user))?;
    // SAFETY: getuid cannot fail.
    let real = unsafe { libc::getuid() };
    if real != 0 {
        if target.uid != 0 {
            return Err(
                "only root may become another user; a member of wheel may become root".to_owned(),
            );
        }
        if asked.shell.is_some() {
            return Err("only root may choose the shell".to_owned());
        }
        let caller = account::name_of(real, &passwd)
            .ok_or_else(|| format!("uid {real} has no account in /etc/passwd"))?;
        prove(real, &caller)?;
    }
    if !target.may_log_in() && asked.shell.is_none() {
        return Err(format!("{} may not log in", target.name));
    }
    account::drop_to(&target).map_err(|why| format!("becoming {} failed: {why}", target.name))?;
    let shell = asked.shell.clone().unwrap_or_else(|| target.shell.clone());
    let base = shell.rsplit('/').next().unwrap_or("sh").to_owned();
    let mut command = Command::new(&shell);
    let _ = command
        .arg0(if asked.login {
            format!("-{base}")
        } else {
            base
        })
        .env("HOME", &target.home)
        .env("USER", &target.name)
        .env("LOGNAME", &target.name)
        .env("SHELL", &shell)
        .env("PATH", PATH);
    if let Some(term) = term {
        let _ = command.env("TERM", term);
    }
    if let Some(line) = &asked.command {
        let _ = command.arg("-c").arg(line);
    }
    if asked.login {
        let _ = std::env::set_current_dir(&target.home).or_else(|_| std::env::set_current_dir("/"));
    }
    Err(format!("{shell}: {}", command.exec()))
}

/// The person proves they are `caller` (uid `real`) to `authd`'s `su`
/// service: anything but an ACCEPTED for exactly them is an error, and root
/// is not taken back.
fn prove(real: u32, caller: &str) -> Result<(), String> {
    // U5: no terminal, no password: refused before connecting. The secret
    // is read from standard input, and only when that is a terminal;
    // `/dev/tty` is the console on Ferrix whatever the caller's terminal is.
    // SAFETY: isatty reads nothing through its argument.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } != 1 {
        return Err("no terminal to ask the password on".to_owned());
    }
    let connection = connect_as(real)?;
    talks_to_authd(&connection)?;
    let verdict = converse(&connection, "su", "", &mut Terminal::tty_only())
        .map_err(|error| format!("cannot reach authd: {error}"))?;
    accepted(&verdict, real, caller)
}

/// U1: whether whoever listens at the other end of `connection` is the
/// init or `authd`, by the uid `SO_PEERCRED` recorded; a server a user
/// started is refused before a byte is sent.
fn talks_to_authd(connection: &Connection) -> Result<(), String> {
    let listener = connection
        .peer_uid()
        .map_err(|error| format!("cannot tell who listens at authd's socket: {error}"))?;
    if AUTHD_UIDS.contains(&listener) {
        Ok(())
    } else {
        Err(format!(
            "the authentication socket is held by uid {listener}, not authd"
        ))
    }
}

/// U3: whether `verdict` lets `caller` (uid `real`) become root.
fn accepted(verdict: &Verdict, real: u32, caller: &str) -> Result<(), String> {
    match verdict {
        Verdict::Accepted { uid, account } if *uid == real && account == caller => Ok(()),
        Verdict::Accepted { .. } => Err("authd accepted someone else".to_owned()),
        Verdict::Failed { .. } => Err("Authentication failure".to_owned()),
        Verdict::Unavailable(text) => Err(text.clone()),
    }
}

/// Connect to `authd` with the effective uid set back to `real` for the
/// connect alone (the certification consultant's U4), so `SO_PEERCRED`
/// names the person; root is taken back at once from the saved uid.
fn connect_as(real: u32) -> Result<Connection, String> {
    let keep = libc::uid_t::MAX;
    // SAFETY: setresuid takes plain integers; `-1` leaves an id as it is.
    if unsafe { libc::setresuid(keep, real, keep) } != 0 {
        return Err(format!(
            "setting the euid: {}",
            std::io::Error::last_os_error()
        ));
    }
    let connection = Connection::open();
    // SAFETY: as above; the saved uid is 0, so this is allowed.
    let back = unsafe { libc::setresuid(keep, 0, keep) };
    if back != 0 {
        return Err(format!(
            "taking the euid back: {}",
            std::io::Error::last_os_error()
        ));
    }
    connection.map_err(|error| format!("cannot reach authd: {error}"))
}

/// Remove every variable from the environment.
fn clear_environment() {
    let names: Vec<std::ffi::OsString> = std::env::vars_os().map(|(name, _)| name).collect();
    for name in names {
        // SAFETY: `su` is one thread, and nothing has read the environment
        // since it started but this.
        unsafe { std::env::remove_var(name) };
    }
}

/// Say a line on standard error.
fn say(line: &str) {
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// U3: root only for an ACCEPTED naming the caller itself.
    #[test]
    fn root_comes_back_only_for_the_callers_own_acceptance() {
        let ok = Verdict::Accepted {
            uid: 1000,
            account: "ferrix".to_owned(),
        };
        assert!(accepted(&ok, 1000, "ferrix").is_ok());
        assert!(accepted(&ok, 1001, "ferrix").is_err());
        assert!(accepted(&ok, 1000, "other").is_err());
        let root = Verdict::Accepted {
            uid: 0,
            account: "root".to_owned(),
        };
        assert!(accepted(&root, 1000, "ferrix").is_err());
        let failed = Verdict::Failed {
            retry_after_ms: 0,
            text: "Authentication failed".to_owned(),
        };
        assert!(accepted(&failed, 1000, "ferrix").is_err());
        assert!(accepted(&Verdict::Unavailable("no".to_owned()), 1000, "ferrix").is_err());
    }

    /// The certification consultant's V1: a server of a user's own -- here
    /// the test's, at the other end of a socket pair -- is refused.
    #[test]
    fn a_server_a_user_started_is_refused() {
        // SAFETY: getuid cannot fail.
        let own = unsafe { libc::getuid() };
        if AUTHD_UIDS.contains(&own) {
            // As root or `auth` the pair's peer would be one of authd's own
            // uids: there is nothing to refuse.
            return;
        }
        let (ours, _theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let connection = Connection::from_fd(std::os::fd::OwnedFd::from(ours));
        let refused = talks_to_authd(&connection);
        assert!(
            refused
                .as_ref()
                .is_err_and(|why| why.contains(&format!("uid {own}"))),
            "{refused:?}"
        );
    }
}
