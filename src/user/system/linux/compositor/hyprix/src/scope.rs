//! A scope for each program the compositor starts (`docs/INIT.md` §5.6).
//!
//! Under Ferrix's init the compositor is `hyprix.service`, and what it
//! starts -- `exec-once`, `exec`, a keybind's program -- would otherwise be
//! in the compositor's own cgroup, charged to it and stopped with it. Each
//! one is asked into `app.slice/app-<name>-<pid>.scope` instead, over the
//! init's control socket, as a desktop session asks systemd for
//! `app-*.scope`: then `svc status` says which window a runaway process came
//! from, and `svc stop` ends all of it. The ask happens on a thread of its
//! own and gives up after two seconds, so a slow or missing init never
//! holds a frame; with no init at all there is no socket, and the program
//! simply stays where it started.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use ferrix_svc_proto::control::{Answer, Call, Framer, SOCKET};

/// The slice the scopes go under, systemd's own name for it.
pub const SLICE: &str = "app.slice";

/// The scope `program`, started as `pid`, is asked into.
#[must_use]
pub fn name(program: &str, pid: u32) -> String {
    let base = program.rsplit('/').next().unwrap_or(program);
    // A unit name takes letters, digits and `:_.-`; anything else is `_`.
    let base: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let base = if base.is_empty() {
        "program".to_owned()
    } else {
        base
    };
    format!("app-{base}-{pid}.scope")
}

/// Ask the init to group `pid`, just started from `program`, in a scope of
/// its own; on a thread, so the caller never waits for it.
pub fn group(program: &str, pid: u32) {
    let unit = name(program, pid);
    let _ = std::thread::Builder::new()
        .name("scope".to_owned())
        .spawn(move || {
            let _ = ask(unit, pid);
        });
}

/// Send the call and read the init's answer, which is not needed: a scope
/// that cannot be made leaves the program where it is.
fn ask(unit: String, pid: u32) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(SOCKET)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let call = Call::Scope {
        unit,
        slice: Some(SLICE.to_owned()),
        pids: vec![pid],
        seat: false,
    };
    stream.write_all(&call.encode())?;
    let mut framer = Framer::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = stream.read(&mut buffer)?;
        if count == 0 {
            return Ok(());
        }
        framer.push(buffer.get(..count).unwrap_or_default());
        while let Ok(Some(record)) = framer.next_record() {
            if Answer::decode(&record).is_ok_and(|answer| answer.is_final()) {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::name;

    #[test]
    fn a_scope_is_named_after_the_program_and_its_pid() {
        assert_eq!(name("/bin/pattern", 41), "app-pattern-41.scope");
        assert_eq!(name("foot", 7), "app-foot-7.scope");
        assert_eq!(name("/bin/my prog+x", 9), "app-my_prog_x-9.scope");
        assert_eq!(name("", 3), "app-program-3.scope");
    }
}
