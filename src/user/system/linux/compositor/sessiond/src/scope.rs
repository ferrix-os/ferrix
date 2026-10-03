//! The graphical session's scope (`docs/AUTH.md` §6.4, P2.7):
//! `user-<uid>.slice/session-<n>.scope`, with the compositor in it, so
//! every program of the session is in one cgroup and ends with it.
//!
//! `n` comes from the count `login` keeps for the console's sessions,
//! `/run/ferrix/login/next`, under the same `flock`, so a boot's sessions
//! are numbered once whichever way they started. A scope the init refuses
//! is not a session: `sessiond` then stops the compositor and starts none,
//! so the session's end may rely on the scope.

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ferrix_svc_proto::control::{Answer, Call, Framer, SOCKET};

/// Where the count is kept, `login`'s directory.
const DIRECTORY: &str = "/run/ferrix/login";

/// Put `pid`, the compositor, in a new session scope of `uid`'s: the
/// scope's unit name, or why not.
pub(crate) fn join(uid: u32, pid: u32) -> Result<String, String> {
    let n = next().map_err(|error| format!("{DIRECTORY}: {error}"))?;
    let unit = format!("session-{n}.scope");
    ask(&Call::Scope {
        unit: unit.clone(),
        slice: Some(format!("user-{uid}.slice")),
        pids: vec![pid],
    })?;
    Ok(unit)
}

/// End every process of the session: the init stops the scope, which
/// signals and then writes `cgroup.kill`. Should the init not answer, or
/// refuse, `sessiond` is root and writes the scope's `cgroup.kill` itself
/// (the certification consultant's Q2); should that fail too, the session's
/// processes may live on, and that is an error said, never a quiet end.
pub(crate) fn end(uid: u32, unit: &str) -> Result<(), String> {
    let asked = match ask(&Call::Stop(unit.to_owned())) {
        Ok(()) => {
            crate::say(&format!("the session's processes were stopped ({unit})"));
            return Ok(());
        }
        Err(why) => why,
    };
    let kill = kill_file(uid, unit);
    match std::fs::write(&kill, b"1") {
        Ok(()) => {
            crate::say(&format!(
                "the init did not stop {unit} ({asked}); {kill} was written, and the session's \
                 processes were stopped"
            ));
            Ok(())
        }
        Err(error) => Err(format!(
            "the session's processes may live on: the init did not stop {unit} ({asked}), and \
             {kill} could not be written ({error})"
        )),
    }
}

/// The scope's own `cgroup.kill`, where the init's cgroup tree puts it.
pub(crate) fn kill_file(uid: u32, unit: &str) -> String {
    format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/{unit}/cgroup.kill")
}

/// The next session number.
fn next() -> std::io::Result<u64> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(DIRECTORY)?;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(format!("{DIRECTORY}/next"))?;
    #[expect(
        unsafe_code,
        reason = "AUDIT: flock on a descriptor this process holds, with constants"
    )]
    // SAFETY: as the reason says.
    let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if locked != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text)?;
    let n = text.trim().parse::<u64>().unwrap_or(0).saturating_add(1);
    file.rewind()?;
    file.set_len(0)?;
    file.write_all(format!("{n}\n").as_bytes())?;
    Ok(n)
}

/// Send `call` and wait for the init's final answer.
fn ask(call: &Call) -> Result<(), String> {
    let mut stream = UnixStream::connect(SOCKET).map_err(|error| format!("{SOCKET}: {error}"))?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    stream
        .write_all(&call.encode())
        .map_err(|error| format!("{SOCKET}: {error}"))?;
    let mut framer = Framer::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("{SOCKET}: {error}"))?;
        if count == 0 {
            return Err("the init closed the socket without an answer".to_owned());
        }
        framer.push(buffer.get(..count).unwrap_or_default());
        while let Ok(Some(record)) = framer.next_record() {
            if let Some(result) = answered(Answer::decode(&record).ok()) {
                return result;
            }
        }
    }
}

/// What an answer means for the session: `None` while more is to come.
fn answered(answer: Option<Answer>) -> Option<Result<(), String>> {
    match answer {
        Some(Answer::Refused(why)) => Some(Err(format!("the init refused: {why}"))),
        Some(answer) if answer.is_final() => Some(Ok(())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The consultant's control (c): a refusal is never taken for a scope.
    #[test]
    fn the_fallback_kills_the_scope_the_init_made() {
        assert_eq!(
            kill_file(1000, "session-3.scope"),
            "/sys/fs/cgroup/user.slice/user-1000.slice/session-3.scope/cgroup.kill"
        );
    }

    #[test]
    fn a_refusal_is_no_scope_and_a_note_is_not_an_answer() {
        assert!(matches!(
            answered(Some(Answer::Refused("Permission denied".to_owned()))),
            Some(Err(_))
        ));
        assert_eq!(
            answered(Some(Answer::Done("done".to_owned()))),
            Some(Ok(()))
        );
        assert_eq!(answered(Some(Answer::Note("made".to_owned()))), None);
        assert_eq!(answered(None), None);
    }
}
