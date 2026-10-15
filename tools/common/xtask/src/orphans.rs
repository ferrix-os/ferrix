//! Leave nothing running when this program dies of a panic.
//!
//! xtask is built with the workspace's `panic = "abort"`, which the kernel
//! needs and which a profile override cannot change for one package. So a
//! panic runs no destructor: the `Child` that owns a QEMU is never killed or
//! waited for, and QEMU runs on, reparented, with the boot's disk images
//! open and write-locked. The next boot of the same worktree then cannot
//! open them, and the orphan has to be found and killed by hand.
//!
//! Seen on 2026-09-26 in an AArch64 `test-powerfail` under the coverage
//! plugin. `/tmp` had filled its quota -- the compositor tests' leaked
//! frames, since fixed -- and the gate's log was there, so the next
//! `println!` of a serial line panicked with "failed printing to stdout" and
//! the process aborted, core dump and all. The coverage run that had started
//! the gate aborted the same way two seconds later, and QEMU was left holding
//! `btrfs-write.img`. It reproduces without a full disk: pipe the gate's
//! output into a reader that leaves after the firmware's first line, and the
//! next line is "failed printing to stdout: Broken pipe".
//!
//! The panic hook therefore kills every process descended from this one,
//! after the usual message: QEMU, a gate a coverage run started, and the
//! QEMU that gate started. Only this program's own descendants, found by
//! their parent ids, so nothing another session runs is touched. A kill by
//! a signal the program cannot catch, SIGKILL, still leaves them; nothing
//! short of each child being told of its parent's death could help that.

use std::io::Write as _;
use std::process::{Command, Stdio};

/// Kill this program's descendants whenever it panics, after the panic's
/// own message.
pub(crate) fn install() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        kill_descendants();
    }));
}

/// Watch this program's parent, and when it is gone -- this process
/// reparented, as when `check` or `check-side` is killed -- kill every
/// process descended from this one and exit (the certification
/// consultant's condition 1, ledger line 1684): `check-side` and each unit
/// it runs as a child call this, so a killed `check` leaves no unit, cargo
/// or test running. Looked at once a second; off Unix, nothing.
pub(crate) fn exit_with_parent() {
    #[cfg(unix)]
    {
        let parent = std::os::unix::process::parent_id();
        // Started by a parent already gone: reparented to init before the
        // watch began, so no change would ever be seen.
        let gone_already = parent == 1;
        let _ = std::thread::spawn(move || {
            if gone_already {
                kill_descendants();
                #[expect(
                    clippy::exit,
                    reason = "nobody waits for this process; it was orphaned before it began"
                )]
                std::process::exit(1);
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                if std::os::unix::process::parent_id() != parent {
                    let _ = writeln!(
                        std::io::stderr(),
                        "xtask: the process that started this one is gone"
                    );
                    kill_descendants();
                    #[expect(
                        clippy::exit,
                        reason = "nobody waits for this process any more; its work is killed"
                    )]
                    std::process::exit(1);
                }
            }
        });
    }
}

/// Every process below this one, killed at once. On Linux, from `/proc`;
/// nothing elsewhere, where `/proc` is not there to read.
fn kill_descendants() {
    let doomed = descendants(std::process::id(), &parents());
    if doomed.is_empty() {
        return;
    }
    // Through `kill(1)`, since this program links no libc crate.
    let _ = Command::new("kill")
        .arg("-KILL")
        .args(doomed.iter().map(u32::to_string))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // Not `eprintln!`, which panics when it cannot write -- and the stream
    // that could not be written may be why this program is panicking.
    let _ = writeln!(
        std::io::stderr(),
        "xtask: killed {} process(es) this run started, so none is left holding its files",
        doomed.len()
    );
}

/// Each process's id and its parent's, as `/proc/<pid>/stat` says.
fn parents() -> Vec<(u32, u32)> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| {
            let pid: u32 = entry.ok()?.file_name().to_str()?.parse().ok()?;
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            Some((pid, parent_in(&stat)?))
        })
        .collect()
}

/// The parent id in a `/proc/<pid>/stat` line: the second field after the
/// name, which is in parentheses and may hold anything, spaces included.
fn parent_in(stat: &str) -> Option<u32> {
    stat.rsplit_once(')')?
        .1
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Every process below `root` in the tree `parents` describes, parents
/// before their children.
fn descendants(root: u32, parents: &[(u32, u32)]) -> Vec<u32> {
    let mut found = Vec::new();
    let mut next = 0;
    let mut parent = root;
    loop {
        for &(pid, ppid) in parents {
            if ppid == parent && pid != root && !found.contains(&pid) {
                found.push(pid);
            }
        }
        let Some(&below) = found.get(next) else {
            return found;
        };
        parent = below;
        next += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{descendants, parent_in};

    #[test]
    fn the_parent_is_read_past_a_name_with_spaces_and_parentheses() {
        assert_eq!(parent_in("42 (qemu (x) y) S 7 42 42 0"), Some(7));
        assert_eq!(parent_in("42 (sh) R 1 42"), Some(1));
        assert_eq!(parent_in("garbage"), None);
    }

    #[test]
    fn descendants_are_every_generation_and_nothing_else() {
        // 10 is this program; 11 a gate it started, 12 that gate's QEMU,
        // 13 a second child; 20 and 21 belong to someone else.
        let tree = [(11, 10), (12, 11), (13, 10), (20, 1), (21, 20), (10, 5)];
        assert_eq!(descendants(10, &tree), [11, 13, 12]);
        assert!(descendants(21, &tree).is_empty());
    }
}
