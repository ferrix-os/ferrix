//! What the read-side files print, byte for byte as Linux prints them.

use crate::write::Limit;
use alloc::vec::Vec;
use core::fmt::Write as _;

/// A `Vec<u8>` that `write!` can format into.
struct Out<'a>(&'a mut Vec<u8>);

impl core::fmt::Write for Out<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        self.0.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// Append `cgroup.procs` or `cgroup.threads`: one id a line, in the order
/// given. cgroup v2 promises no order -- only v1's `tasks` was sorted -- and
/// Ferrix's is the registry's, ascending.
pub fn ids(out: &mut Vec<u8>, ids: &[u32]) {
    for &id in ids {
        let _ = writeln!(Out(out), "{id}");
    }
}

/// Append `cgroup.events`.
pub fn events(out: &mut Vec<u8>, populated: bool, frozen: bool) {
    let _ = write!(
        Out(out),
        "populated {}\nfrozen {}\n",
        u8::from(populated),
        u8::from(frozen)
    );
}

/// Append `cgroup.stat`: the cgroups beneath this one, and the dying ones --
/// removed but still pinned -- which Ferrix never has, since a removed
/// cgroup is a job that can be dropped at once.
pub fn stat(out: &mut Vec<u8>, descendants: u32) {
    let _ = write!(
        Out(out),
        "nr_descendants {descendants}\nnr_dying_descendants 0\n"
    );
}

/// Append a limit as `cgroup.max.depth` and `cgroup.max.descendants` print
/// it.
pub fn limit(out: &mut Vec<u8>, limit: Limit) {
    match limit {
        Limit::Max => out.extend_from_slice(b"max\n"),
        Limit::At(count) => {
            let _ = writeln!(Out(out), "{count}");
        }
    }
}

/// Append a cgroup's path as the cgroup v2 hierarchy names it: `/` for the
/// root, `/a/b` beneath it. `names` goes from the root's child down.
pub fn path<'a>(out: &mut Vec<u8>, names: impl IntoIterator<Item = &'a [u8]>) {
    let mut any = false;
    for name in names {
        out.push(b'/');
        out.extend_from_slice(name);
        any = true;
    }
    if !any {
        out.push(b'/');
    }
}

/// Append `/proc/<pid>/cgroup` for a process in the cgroup `names` leads
/// to: the unified hierarchy's line, `0::/path`, and no other, since there
/// are no v1 hierarchies.
pub fn proc_cgroup<'a>(out: &mut Vec<u8>, names: impl IntoIterator<Item = &'a [u8]>) {
    out.extend_from_slice(b"0::");
    path(out, names);
    out.push(b'\n');
}

/// Append `pids.max` or `memory.max`: `max` for no limit, else the number.
pub fn max(out: &mut Vec<u8>, limit: Option<u64>) {
    match limit {
        None => out.extend_from_slice(b"max\n"),
        Some(value) => {
            let _ = writeln!(Out(out), "{value}");
        }
    }
}

/// Append a single number and a newline: `pids.current`, `memory.current`,
/// `cpu.weight`.
pub fn number(out: &mut Vec<u8>, value: u64) {
    let _ = writeln!(Out(out), "{value}");
}

/// Append `pids.events`: how many forks the limit refused.
pub fn pids_events(out: &mut Vec<u8>, max: u64) {
    let _ = writeln!(Out(out), "max {max}");
}

/// What `memory.stat` prints: only the keys Ferrix has a source for, in
/// Linux's order, and a reader looks a key up by its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryStat {
    /// Bytes of the cgroup's pages in the page cache of files on a disk.
    pub file: u64,
    /// Bytes of kernel memory held for the cgroup's programs: the heap part
    /// of `memory.current`.
    pub kernel: u64,
    /// Bytes of the cgroup's pages in a memory filesystem's files (tmpfs,
    /// `memfd`).
    pub shmem: u64,
    /// Pages reclaim looked at for the cgroup.
    pub pgscan: u64,
    /// Pages reclaim gave back from the cgroup.
    pub pgsteal: u64,
    /// Page faults the cgroup's programs took.
    pub pgfault: u64,
    /// Page faults that read a file from its disk.
    pub pgmajfault: u64,
}

/// Append `memory.stat`. Linux prints some sixty keys; a key with no source
/// here is left out and not printed as zero.
pub fn memory_stat(out: &mut Vec<u8>, stat: MemoryStat) {
    let _ = write!(
        Out(out),
        "file {}
kernel {}
shmem {}
pgscan {}
pgsteal {}
pgfault {}
pgmajfault {}
",
        stat.file,
        stat.kernel,
        stat.shmem,
        stat.pgscan,
        stat.pgsteal,
        stat.pgfault,
        stat.pgmajfault
    );
}

/// What `memory.events` counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MemoryEvents {
    /// Times the cgroup was reclaimed from below `memory.low`: never, since
    /// protection is reported and not enforced.
    pub low: u64,
    /// Times it went over `memory.high` and was reclaimed.
    pub high: u64,
    /// Charges `memory.max` refused.
    pub max: u64,
    /// Faults that found `memory.max` full and asked for a kill.
    pub oom: u64,
    /// Processes the scoped OOM kill ended.
    pub oom_kill: u64,
}

/// Append `memory.events`, in the order Linux prints it; `oom_group_kill`,
/// which Ferrix never counts (no `memory.oom.group`), as zero.
pub fn memory_events(out: &mut Vec<u8>, events: MemoryEvents) {
    let _ = write!(
        Out(out),
        "low {}
high {}
max {}
oom {}
oom_kill {}
oom_group_kill 0
",
        events.low,
        events.high,
        events.max,
        events.oom,
        events.oom_kill
    );
}
