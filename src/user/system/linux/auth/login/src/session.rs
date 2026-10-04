//! The session's scope (`docs/INIT.md` §5.6): `session-<n>.scope` under
//! `user-<uid>.slice`, asked of the init while `login` is still root, with
//! `login` itself in it, so the shell it execs and everything that shell
//! starts are the session's.
//!
//! `n` counts from 1 at every boot, in `/run/ferrix/login/next`, a file in a
//! directory only root may enter, under `flock` so that two consoles never
//! take the same number.
//!
//! A session on the console is also named in `/run/ferrix/login/console`,
//! and the next `login` on the console stops it before it asks who is there:
//! a program the last person left running there, in their scope or in one
//! of its own under their slice, ends with it. getty's hangup has already
//! taken the console from it (`docs/AUTH.md` §1); this ends the rest.

use std::io::{Read as _, Seek as _, Write as _};
use std::os::fd::AsRawFd as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ferrix_svc_proto::control::{Answer, Call, Framer, SOCKET};

/// Where the count is kept.
const DIRECTORY: &str = "/run/ferrix/login";

/// Put this process in a new session scope of `uid`'s: its name, or why
/// not.
/// Its unit's name and its slice's.
pub(crate) fn join(uid: u32) -> Result<(String, String), String> {
    let n = next().map_err(|error| format!("{DIRECTORY}: {error}"))?;
    let unit = format!("session-{n}.scope");
    let slice = format!("user-{uid}.slice");
    ask(Call::Scope {
        unit: unit.clone(),
        slice: Some(slice.clone()),
        pids: vec![std::process::id()],
    })?;
    Ok((slice, unit))
}

/// Where the console's session is named.
const CONSOLE: &str = "/run/ferrix/login/console";

/// `/dev/console`'s device number, 5:1, as `fstat` reports it.
const CONSOLE_RDEV: u64 = (5 << 8) | 1;

/// Whether standard input is the console.
pub(crate) fn on_console() -> bool {
    // SAFETY: an all-zero `stat` is a valid value for `fstat` to fill.
    let mut status: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: descriptor 0 and a `stat` this frame owns.
    let ok = unsafe { libc::fstat(0, &raw mut status) } == 0;
    ok && status.st_mode & libc::S_IFMT == libc::S_IFCHR && status.st_rdev == CONSOLE_RDEV
}

/// Where the init's cgroup tree puts a user's slices.
const USER_SLICE: &str = "/sys/fs/cgroup/user.slice";

/// Name `slice/unit` as the console's session, for the next `login` there
/// to end.
pub(crate) fn remember_console(slice: &str, unit: &str) -> Result<(), String> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(CONSOLE)
        .and_then(|mut file| file.write_all(format!("{slice}/{unit}\n").as_bytes()))
        .map_err(|error| format!("{CONSOLE}: {error}"))
}

/// Stop the console's last session, if one is named and its scope is still
/// there: the scope's unit, or `None` if there was nothing to stop. The
/// name is forgotten either way, once asked about: a session that cannot be
/// stopped is said once, not at every login after.
pub(crate) fn end_console() -> Result<Option<String>, String> {
    let named = match std::fs::read_to_string(CONSOLE) {
        Ok(text) => text.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{CONSOLE}: {error}")),
    };
    let forgotten = std::fs::remove_file(CONSOLE).map_err(|error| format!("{CONSOLE}: {error}"));
    let Some((slice, unit)) = named.split_once('/').filter(|(slice, unit)| {
        is_numbered(slice, "user-", ".slice") && is_numbered(unit, "session-", ".scope")
    }) else {
        return Err(format!("{CONSOLE} names no session"));
    };
    // A scope whose processes have all ended is gone from the tree, and
    // there is nothing to stop.
    if !std::path::Path::new(&format!("{USER_SLICE}/{slice}/{unit}")).exists() {
        return forgotten.map(|()| None);
    }
    ask(Call::Stop(unit.to_owned()))?;
    forgotten.map(|()| Some(unit.to_owned()))
}

/// Whether `name` is `prefix`, a decimal number, `suffix`: the names
/// [`join`] gives.
fn is_numbered(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|byte| byte.is_ascii_digit()))
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
    // SAFETY: flock on a descriptor this process holds, with constants.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text)?;
    let n = text.trim().parse::<u64>().unwrap_or(0).saturating_add(1);
    let _ = file.rewind();
    file.set_len(0)?;
    file.write_all(format!("{n}\n").as_bytes())?;
    Ok(n)
}

/// Send `call` and wait for the init's final answer.
fn ask(call: Call) -> Result<(), String> {
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
            match Answer::decode(&record) {
                Ok(Answer::Refused(why)) => return Err(format!("the init refused: {why}")),
                Ok(answer) if answer.is_final() => return Ok(()),
                _ => {}
            }
        }
    }
}
