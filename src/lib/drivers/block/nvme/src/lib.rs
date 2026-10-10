//! An NVMe controller over PCIe, as logic over registers and memory.
//!
//! `src/user/system/native/drivers/block/nvme` runs this in a ring-3 process
//! under devmgr and serves the controller's first namespace over the block
//! ring (`docs/BLOCK-RING.md`, `docs/NVME.md`). The process holds an
//! `IoMapping` of the controller's registers, memory pinned for it and a
//! clock; none of them exists in a unit test, so everything that decides a
//! register value, a command's bytes or a queue index is here, written
//! against [`Registers`], [`Memory`] and [`Clock`], and tested against a
//! model of a controller (`tests`).
//!
//! * [`regs`]: the register offsets and bits used;
//! * [`command`]: submission and completion queue entries;
//! * [`identify`]: the Identify structures' fields;
//! * [`queue`]: a queue pair's head, tail and phase;
//! * [`prp`]: the page lists that say where a command's data is;
//! * [`hmb`]: the Host Memory Buffer a drive without DRAM asks for;
//! * [`quirks`]: what particular controllers need;
//! * [`driver`]: the controller's life -- reset, enable, Identify, the
//!   queues, reads, writes and flushes, timeouts, and the clean shutdown.
//!
//! # Where the numbers come from
//!
//! Register offsets, opcodes, feature identifiers and structure layouts are
//! the NVM Express Base Specification's (revision 2.0, unchanged for these
//! fields since 1.0 to 1.3) and the NVM Command Set Specification's, each
//! checked against the layouts Linux's `include/linux/nvme.h` and QEMU's
//! `include/block/nvme.h` declare. The quirks are the facts Linux's driver
//! records for the devices named. Both are GPL-2.0; none of their code is
//! here.
//!
//! # Memory the controller reaches
//!
//! Three regions, each whole 4 KiB pages the process pinned:
//!
//! * the *queues*: for the admin queue and each I/O queue, one page of
//!   submission entries and then one page of completion entries, so no
//!   queue needs two pages next to each other;
//! * the *lists*: page 0 is scratch for Identify and feature data, page 1
//!   holds the host memory descriptor list, and after them each command
//!   slot of each I/O queue has its own PRP list pages;
//! * the *data*: the block ring's data region, which the kernel fills and
//!   reads and the controller transfers straight into and out of.
//!
//! The pages' device addresses need not be contiguous, and no code here
//! assumes they are.

#![no_std]
#![forbid(unsafe_code)]

pub mod command;
pub mod driver;
pub mod hmb;
pub mod identify;
pub mod prp;
pub mod queue;
pub mod quirks;
pub mod regs;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

pub use driver::{
    Cause, Completion, Drained, Driver, Event, Failed, Fatal, InitError, Op, Options, Outcome,
    Parts, Request, Shutdown, Stats, SubmitError, Submitted,
};

/// The controller's registers, in its first BAR.
///
/// A 64-bit register is read and written as two 32-bit halves, low first,
/// which the specification allows and every bus can do.
pub trait Registers {
    /// Read the 32-bit register at `offset`.
    fn read(&self, offset: u32) -> u32;
    /// Write the 32-bit register at `offset`.
    fn write(&mut self, offset: u32, value: u32);
}

/// Time, for timeouts and the waits between polls.
pub trait Clock {
    /// Nanoseconds on a clock that only moves forward.
    fn now_nanos(&self) -> u64;
    /// Wait about `nanos` between two polls of a condition. The process
    /// sleeps; a test's clock moves on.
    fn pause(&mut self, nanos: u64);
}

/// Whole pages of memory both this crate and the controller reach.
pub trait Memory {
    /// The device address of each 4 KiB page, in order.
    fn pages(&self) -> &[u64];
    /// The byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;
    /// Write the byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);
    /// The little-endian `u16` at `offset`, a multiple of two, in one
    /// access: the controller may be writing it.
    fn read_u16(&self, offset: usize) -> u16;
    /// Order every write made so far before whatever is written next, as
    /// the controller sees them: called before a doorbell.
    fn barrier(&self) {}
}
