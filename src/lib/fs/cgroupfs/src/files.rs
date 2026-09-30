//! The interface files of a cgroup that belong to cgroup itself, not to a
//! controller.
//!
//! Linux's `cgroup_base_files`, in its order, with the ones it marks
//! `CFTYPE_NOT_ON_ROOT` marked here too: the root has no type, no events, no
//! freeze and no kill, because nothing can be above it to judge them by and
//! killing it would kill the machine.
//!
//! A controller's files follow, each marked with its controller: a cgroup
//! has them only while its parent's `cgroup.subtree_control` enables it, and
//! the root never, as on Linux, whose root has no limits to set.

use crate::controllers::{Controller, Set};

/// One file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    /// Its name in the cgroup's directory.
    pub name: &'static str,
    /// What it is.
    pub kind: Kind,
    /// Whether the root cgroup has it.
    pub on_root: bool,
    /// Whether it can be written; `false` means read-only, mode 0444.
    pub writable: bool,
    /// The controller it belongs to, or `None` for cgroup's own.
    pub controller: Option<Controller>,
}

/// Which file a [`File`] is, for the kernel to dispatch on without comparing
/// names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `cgroup.type`.
    Type,
    /// `cgroup.procs`.
    Procs,
    /// `cgroup.threads`.
    Threads,
    /// `cgroup.controllers`.
    Controllers,
    /// `cgroup.subtree_control`.
    SubtreeControl,
    /// `cgroup.events`.
    Events,
    /// `cgroup.max.descendants`.
    MaxDescendants,
    /// `cgroup.max.depth`.
    MaxDepth,
    /// `cgroup.stat`.
    Stat,
    /// `cgroup.freeze`.
    Freeze,
    /// `cgroup.kill`, which is write-only on Linux: mode 0200.
    Kill,
    /// `cpu.stat`.
    CpuStat,
    /// `cpu.weight`.
    CpuWeight,
    /// `cpu.weight.nice`.
    CpuWeightNice,
    /// `cpu.max`.
    CpuMax,
    /// `memory.current`.
    MemoryCurrent,
    /// `memory.max`.
    MemoryMax,
    /// `memory.high`: the mark above which a cgroup's pages are reclaimed.
    MemoryHigh,
    /// `memory.low`: the best-effort protection from reclaim.
    MemoryLow,
    /// `memory.min`: the hard protection from reclaim.
    MemoryMin,
    /// `memory.events`.
    MemoryEvents,
    /// `memory.stat`: the keys Ferrix has a source for.
    MemoryStat,
    /// `io.stat`.
    IoStat,
    /// `io.max`.
    IoMax,
    /// `pids.current`.
    PidsCurrent,
    /// `pids.max`.
    PidsMax,
    /// `pids.events`.
    PidsEvents,
}

/// Every file, in Linux's order.
pub const FILES: &[File] = &[
    file("cgroup.type", Kind::Type, false, true),
    file("cgroup.procs", Kind::Procs, true, true),
    file("cgroup.threads", Kind::Threads, true, true),
    file("cgroup.controllers", Kind::Controllers, true, false),
    file("cgroup.subtree_control", Kind::SubtreeControl, true, true),
    file("cgroup.events", Kind::Events, false, false),
    file("cgroup.max.descendants", Kind::MaxDescendants, true, true),
    file("cgroup.max.depth", Kind::MaxDepth, true, true),
    file("cgroup.stat", Kind::Stat, true, false),
    file("cgroup.freeze", Kind::Freeze, false, true),
    file("cgroup.kill", Kind::Kill, false, true),
    file("cpu.stat", Kind::CpuStat, true, false),
    controlled("cpu.weight", Kind::CpuWeight, Controller::Cpu, true),
    controlled(
        "cpu.weight.nice",
        Kind::CpuWeightNice,
        Controller::Cpu,
        true,
    ),
    controlled("cpu.max", Kind::CpuMax, Controller::Cpu, true),
    controlled(
        "memory.current",
        Kind::MemoryCurrent,
        Controller::Memory,
        false,
    ),
    controlled("memory.min", Kind::MemoryMin, Controller::Memory, true),
    controlled("memory.low", Kind::MemoryLow, Controller::Memory, true),
    controlled("memory.high", Kind::MemoryHigh, Controller::Memory, true),
    controlled("memory.max", Kind::MemoryMax, Controller::Memory, true),
    controlled(
        "memory.events",
        Kind::MemoryEvents,
        Controller::Memory,
        false,
    ),
    controlled("memory.stat", Kind::MemoryStat, Controller::Memory, false),
    on_root_too("io.stat", Kind::IoStat, Controller::Io, false),
    controlled("io.max", Kind::IoMax, Controller::Io, true),
    controlled("pids.current", Kind::PidsCurrent, Controller::Pids, false),
    controlled("pids.max", Kind::PidsMax, Controller::Pids, true),
    controlled("pids.events", Kind::PidsEvents, Controller::Pids, false),
];

/// A table entry of cgroup's own.
const fn file(name: &'static str, kind: Kind, on_root: bool, writable: bool) -> File {
    File {
        name,
        kind,
        on_root,
        writable,
        controller: None,
    }
}

/// A table entry of a controller's, which the root never has.
const fn controlled(
    name: &'static str,
    kind: Kind,
    controller: Controller,
    writable: bool,
) -> File {
    File {
        name,
        kind,
        on_root: false,
        writable,
        controller: Some(controller),
    }
}

/// A table entry of a controller's that the root has too: `io.stat`, which
/// counts what the whole machine did.
const fn on_root_too(
    name: &'static str,
    kind: Kind,
    controller: Controller,
    writable: bool,
) -> File {
    File {
        name,
        kind,
        on_root: true,
        writable,
        controller: Some(controller),
    }
}

/// The files a cgroup has: the root's, or every one of cgroup's own and the
/// files of each controller in `enabled`, what its parent's
/// `cgroup.subtree_control` enables.
pub fn of(root: bool, enabled: Set) -> impl Iterator<Item = &'static File> {
    FILES.iter().filter(move |file| {
        (file.on_root || !root)
            && file
                .controller
                .is_none_or(|controller| enabled.contains(controller))
    })
}

/// The file called `name` in a cgroup, if it has one.
pub fn named(name: &[u8], root: bool, enabled: Set) -> Option<&'static File> {
    of(root, enabled).find(|file| file.name.as_bytes() == name)
}

impl File {
    /// Its permission bits: Linux's 0644 for a writable file, 0444 for a
    /// read-only one, and 0200 for `cgroup.kill`, which cannot be read.
    pub fn mode(&self) -> u16 {
        match (self.kind, self.writable) {
            (Kind::Kill, _) => 0o200,
            (_, true) => 0o644,
            (_, false) => 0o444,
        }
    }
}
