//! Telling lode, the log manager, of the session (`docs/LOGGING.md` §19.1,
//! step 2a).
//!
//! `sessiond` owns seat0, so it is the one to say when seat0's session
//! begins and ends. It draws the session's token, a random 16 bytes, hands
//! it to the compositor beside the card (`FERRIX_SESSION_TOKEN`), and writes
//! one line to lode's root-only socket when the session's scope exists and
//! one when it is gone ([`ferrix_lode_core::session`]). lode stamps the
//! session's id on its lines, and later trusts a screenshot provider only
//! if it holds this token and is this pid.
//!
//! Never in the way: a lode that is not running, or that does not answer
//! within [`PATIENCE`], costs one line said, and the session goes on.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ferrix_lode_core::session::{self, Session};

/// lode's socket for the seat's owner.
const SOCKET: &str = "/run/lode/seat";

/// The variable the compositor finds the token in.
pub(crate) const TOKEN_VARIABLE: &str = "FERRIX_SESSION_TOKEN";

/// How long a write to lode may take.
const PATIENCE: Duration = Duration::from_millis(500);

/// A fresh token, from the kernel's random numbers.
pub(crate) fn token() -> [u8; 16] {
    let mut token = [0_u8; 16];
    if let Ok(mut random) = std::fs::File::open("/dev/urandom") {
        let _ = random.read_exact(&mut token);
    }
    token
}

/// `token` in hex, as the compositor is given it.
pub(crate) fn hex(token: &[u8; 16]) -> String {
    token.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The unit `sessiond` runs in, whose output the session's lines come
/// through: the last part of its cgroup's path.
fn own_unit() -> Option<String> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::"))?;
    path.rsplit('/')
        .next()
        .map(str::to_owned)
        .filter(|unit| !unit.is_empty())
}

/// Write `line` to lode; why not, if it could not be.
fn tell(line: &str) -> Result<(), String> {
    let mut stream = UnixStream::connect(SOCKET).map_err(|error| format!("{SOCKET}: {error}"))?;
    stream
        .set_write_timeout(Some(PATIENCE))
        .map_err(|error| format!("{SOCKET}: {error}"))?;
    stream
        .write_all(line.as_bytes())
        .map_err(|error| format!("{SOCKET}: {error}"))
}

/// Say to lode that seat0's session in `scope` began, its compositor `pid`
/// of account `uid`; a line for `sessiond`'s own output says how it went.
pub(crate) fn began(scope: &str, uid: u32, pid: u32, token: &[u8; 16]) -> String {
    let Some(id) = scope
        .strip_prefix("session-")
        .and_then(|rest| rest.strip_suffix(".scope"))
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return format!("lode was not told of the session: {scope} is not session-<n>.scope");
    };
    let line = session::line(&Session {
        id,
        seat: "seat0".to_owned(),
        uid,
        pid,
        scope: scope.to_owned(),
        unit: own_unit().unwrap_or_else(|| "sessiond.service".to_owned()),
        token: *token,
    });
    match tell(&line) {
        Ok(()) => format!("lode was told of session {id}"),
        Err(why) => {
            format!("lode was not told of session {id} ({why}); its lines carry no session id")
        }
    }
}

/// Say to lode that the session in `scope` ended.
pub(crate) fn ended(scope: &str) {
    if let Some(id) = scope
        .strip_prefix("session-")
        .and_then(|rest| rest.strip_suffix(".scope"))
    {
        let _ = tell(&format!("END {id}\n"));
    }
}
