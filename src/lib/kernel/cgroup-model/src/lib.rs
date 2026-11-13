//! The job tree's cgroup rules: the controllers, their sets, the
//! no-internal-process rule, and a subtree's size limit.
//!
//! **The rules are Linux's cgroup v2 rules** (`kernel/cgroup/cgroup.c`), the
//! ones a job, which is a cgroup since stage 13 (`docs/CGROUPS.md`), keeps
//! whatever reads it. The kernel's core holds a job's controllers and limits
//! in these types and refuses by these rules; the text of cgroupfs, the
//! Linux ABI's view of the tree (`ferrix-cgroupfs`), prints and parses them.
//!
//! # Why a crate of its own
//!
//! They were `ferrix-cgroupfs`'s, which kept that crate in a kernel built
//! without the Linux ABI. Here they are a crate with no dependency, and
//! `ferrix_cgroupfs::controllers`, `ferrix_cgroupfs::write::Limit` and
//! `ferrix_cgroupfs::Refusal` re-export these same items: one type under two
//! paths, never a second type or a conversion
//! (`docs/microkernel/CORE-WITHOUT-EXTENSIONS.md` E4, on ledger 9212 Q2.1's
//! terms). The items are as they were in that crate.
//!
//! `no_std`, no dependency, no allocation, no `unsafe`.

#![no_std]
#![forbid(unsafe_code)]

pub mod controllers;

#[cfg(test)]
mod tests;

/// Why a write, or a name, is refused. The kernel turns each into the errno
/// Linux answers with; the mapping is its, since this crate has no errno.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Not a thing the file takes: Linux's `EINVAL`.
    Invalid,
    /// A number the file takes the form of but not the value of: `ERANGE`.
    Range,
    /// Something Linux takes and Ferrix does not do: `EOPNOTSUPP`.
    NotSupported,
    /// A name longer than a directory entry may be: `ENAMETOOLONG`.
    TooLong,
    /// What the no-internal-process rule forbids: `EBUSY`.
    Busy,
}

/// A limit on a subtree's size: `cgroup.max.depth` and
/// `cgroup.max.descendants`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// No limit, written and printed as `max`.
    Max,
    /// At most this many.
    At(u32),
}

impl Limit {
    /// Whether `count` is within it.
    pub fn allows(self, count: u32) -> bool {
        match self {
            Limit::Max => true,
            Limit::At(most) => count <= most,
        }
    }
}
