//! The text of cgroupfs, as pure functions of what the kernel knows.
//!
//! cgroupfs is a view of the job tree for the Linux ABI (`docs/CGROUPS.md`).
//! Its files are an interface in both directions: systemd, a container
//! runtime and `docs/INIT.md`'s init read them with fixed formats and write
//! them with strings they expect Linux to parse in one exact way. So both
//! halves are pinned here, against what Linux prints and against how Linux's
//! `kernel/cgroup/cgroup.c` parses each write, where `cargo test` and the
//! fuzzer reach them -- rather than in the kernel, where the first test of a
//! format would be a service manager misreading its own tree.
//!
//! Nothing here knows about jobs. The kernel gathers the numbers and names,
//! and these functions only arrange or interpret them, as `ferrix-procfs`
//! does for `/proc`.
//!
//! # What is here
//!
//! * [`name`] -- which names `mkdir` may give a cgroup.
//! * [`files`] -- the interface files every cgroup has, and which the root
//!   lacks.
//! * [`controllers`] -- the controller names, `cgroup.controllers`, what a
//!   write to `cgroup.subtree_control` asks for, and the no-internal-process
//!   rule that decides whether it, or a move into a cgroup, may happen.
//! * [`write`](mod@write) -- the writes of a number: `cgroup.procs`, `cgroup.kill`,
//!   `cgroup.max.depth`, `cgroup.max.descendants` and `cgroup.type`.
//! * [`cpu`] -- `cpu.max`, `cpu.weight.nice` and `cpu.stat`.
//! * [`render`] -- `cgroup.procs`, `cgroup.events`, `cgroup.stat`, the limits,
//!   and `/proc/<pid>/cgroup`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod controllers;
pub mod cpu;
pub mod files;
pub mod name;
pub mod render;
pub mod write;

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
