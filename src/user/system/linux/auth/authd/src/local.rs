//! Whether a caller is at the console (`docs/AUTH.md` §5.4,
//! `FirstPassword=local`): read from the kernel, never taken from the caller.
//!
//! `/proc/<pid>/stat`'s seventh field, `tty_nr`, is the controlling
//! terminal's device number as Linux encodes it. Ferrix gives the console's,
//! 5:1, to a process whose session holds the console, and 0 to every other,
//! a pseudo-terminal's session among them (`src/kernel/extensions/personality/linux/src/fs/procfs/render.rs`).
//! Anything else -- no file, a pid gone, a field that does not parse -- is
//! not the console.

/// The console's `tty_nr`: major 5, minor 1, as Ferrix's procfs encodes it
/// (`CONSOLE_TTY_NR`, `5 << 8 | 1`, 1281). `test-init` prints what the
/// console's own shell reads, so the number is seen on the target, not only
/// assumed from Linux's formula.
pub(crate) const CONSOLE: u32 = 5 << 8 | 1;

/// The controlling terminal's `tty_nr` of `pid`, if `pid` is still root's:
/// its real and effective uid read back from `/proc/<pid>/status` beside the
/// `stat` line, so a pid that went and was given to another process is not
/// taken for the caller. `None` when either cannot be read or the uid is not
/// 0.
pub(crate) fn root_terminal(pid: i32) -> Option<u32> {
    if pid <= 0 {
        return None;
    }
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let again = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    (is_roots(&status) && is_roots(&again)).then(|| tty_nr(&stat))?
}

/// Whether a `status` file's `Uid:` line has 0 as its real and effective
/// uid.
pub(crate) fn is_roots(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| {
            let mut ids = ids.split_ascii_whitespace();
            ids.next() == Some("0") && ids.next() == Some("0")
        })
}

/// A `stat` line's `tty_nr`. The command name, in parentheses, may hold
/// spaces and parentheses itself, so the fields are counted from the last
/// `)`.
pub(crate) fn tty_nr(stat: &str) -> Option<u32> {
    let (_, rest) = stat.rsplit_once(')')?;
    // After the name: state, ppid, pgrp, session, tty_nr.
    rest.split_ascii_whitespace().nth(4)?.parse().ok()
}

/// Whether a `stat` line's `tty_nr` is the console's.
#[cfg(test)]
fn is_console(stat: &str) -> bool {
    tty_nr(stat) == Some(CONSOLE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_console_is_local_and_nothing_else_is() {
        assert!(is_console("412 (login) S 1 412 412 1281 412 4194560 0 0"));
        // A pty's session reads 0, as one with no terminal does.
        assert!(!is_console("412 (login) S 1 412 412 0 -1 4194560 0 0"));
        // Linux's pts and ttyS numbers are not the console either.
        assert!(!is_console("412 (login) S 1 412 412 34816 412 0"));
        assert!(!is_console("412 (login) S 1 412 412 1088 412 0"));
        // A name that tries to shift the fields.
        assert!(!is_console("412 (a) S 1 2 3 1281) S 1 412 412 0 -1 0"));
        assert!(is_console("412 (a) b) S 1 412 412 1281 412 0"));
        for bad in ["", "412", "412 (login", "412 (login) S 1 412 412"] {
            assert!(!is_console(bad), "{bad:?}");
        }
        assert_eq!(root_terminal(0), None);
        assert_eq!(root_terminal(-1), None);
    }

    #[test]
    fn only_a_root_pid_is_looked_at() {
        assert!(is_roots("Name:\tlogin\nUid:\t0\t0\t0\t0\n"));
        assert!(!is_roots("Uid:\t1000\t1000\t1000\t1000\n"));
        assert!(!is_roots("Uid:\t0\t1000\t0\t0\n"));
        assert!(!is_roots("Uid:\t1000\t0\t0\t0\n"));
        assert!(!is_roots("Name:\tlogin\n"));
    }
}
