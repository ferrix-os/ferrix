//! Who asked, bound to the connection (`docs/SETTINGS-WRITER.md` §12.5,
//! S3; ledger 3636 (4)): the executable and the start time of the pid
//! `SO_PEERCRED` recorded at `connect`, read when the connection is taken
//! and again once its request has arrived, and the executable of the
//! program that started it, for the prompt's words.

/// A process as `/proc` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Program {
    /// Its executable's path.
    pub(crate) exe: String,
    /// When it started, in clock ticks since boot (`stat` field 22).
    pub(crate) start: u64,
    /// The executable of its parent, or `-` when that cannot be read.
    pub(crate) parent: String,
}

/// Where a process is looked up: `/proc`, or a test's script.
pub(crate) trait Identify: Send + Sync {
    /// The process `pid`, if it is there.
    fn of(&self, pid: i32) -> Option<Program>;
}

/// The machine's `/proc`.
#[derive(Debug)]
pub(crate) struct Proc;

/// `stat`'s fields after the name, which may hold spaces and `)`: from
/// the last `)` on. Field 3 is the first of them.
fn after_name(stat: &str) -> Option<Vec<&str>> {
    let at = stat.rfind(')')?;
    Some(stat.get(at + 1..)?.split_whitespace().collect())
}

/// A `stat` line's parent pid (field 4) and start time (field 22).
pub(crate) fn parent_and_start(stat: &str) -> Option<(i32, u64)> {
    let fields = after_name(stat)?;
    // Field 3 is the state, at index 0.
    let parent = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((parent, start))
}

impl Identify for Proc {
    fn of(&self, pid: i32) -> Option<Program> {
        if pid <= 0 {
            return None;
        }
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (parent, start) = parent_and_start(&stat)?;
        let parent = std::fs::read_link(format!("/proc/{parent}/exe"))
            .map_or_else(|_| "-".to_owned(), |path| path.display().to_string());
        Some(Program {
            exe: exe.to_str()?.to_owned(),
            start,
            parent,
        })
    }
}

/// Whether the program read once the request arrived is the one read when
/// the connection was taken: the same executable, started at the same
/// time. A pid that went, or was used again, is not.
pub(crate) fn bound(at_accept: Option<&Program>, now: Option<&Program>) -> bool {
    match (at_accept, now) {
        (Some(was), Some(is)) => was.exe == is.exe && was.start == is.start,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{Program, bound, parent_and_start};

    #[test]
    fn a_stat_line_s_parent_and_start_are_read_past_its_name() {
        let line = "812 (set tings) )) S 640 812 640 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 98765 1000 10";
        assert_eq!(parent_and_start(line), Some((640, 98_765)));
        assert_eq!(parent_and_start("812 (short) S 1"), None);
    }

    #[test]
    fn a_program_that_changed_or_went_is_not_the_one_that_asked() {
        let program = |exe: &str, start| Program {
            exe: exe.to_owned(),
            start,
            parent: "-".to_owned(),
        };
        let was = program("/bin/settings", 10);
        assert!(bound(Some(&was), Some(&program("/bin/settings", 10))));
        assert!(!bound(Some(&was), Some(&program("/bin/zinc", 10))));
        assert!(!bound(Some(&was), Some(&program("/bin/settings", 11))));
        assert!(!bound(Some(&was), None));
        assert!(!bound(None, Some(&was)));
    }
}
