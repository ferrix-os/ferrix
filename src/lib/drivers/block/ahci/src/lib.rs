//! An AHCI (SATA) host bus adapter, as logic over registers and memory.
//!
//! A ring-3 driver process under devmgr runs this against the HBA's ABAR
//! (BAR 5) and serves the first ATA disk it finds. The process holds an
//! `IoMapping` of the registers, memory pinned for the HBA and a clock;
//! none of them exists in a unit test, so everything that decides a
//! register value or a command's bytes is here, written against
//! [`Registers`], [`Memory`] and [`Clock`], and tested against a model of
//! an HBA (`tests`).
//!
//! * [`regs`]: the register offsets and bits used;
//! * [`command`]: the command header, command FIS and PRDT, and where each
//!   lives in control memory;
//! * [`identify`]: IDENTIFY DEVICE's fields;
//! * [`driver`]: the HBA's life -- handoff, enable, finding the drive,
//!   IDENTIFY, polled reads, writes and flushes, recovery and shutdown.
//!
//! One command at a time, slot 0, polled: no interrupts, no native command
//! queuing. Correctness first. A read may be launched and waited for later
//! ([`Ahci::start_read`], [`Ahci::finish`]), so a driver can read ahead
//! while the kernel takes what came; any other command waits for it first.
//!
//! # Where the numbers come from
//!
//! Register offsets, bits and structure layouts are the Serial ATA AHCI
//! 1.3.1 specification's; opcodes and IDENTIFY DEVICE's words are
//! ATA8-ACS's. None of Linux's or QEMU's code is here.
//!
//! # Memory the HBA reaches
//!
//! * the *control* memory, [`CONTROL_PAGES`] whole 4 KiB pages: the
//!   command list, received FIS area and command table in the first,
//!   IDENTIFY's data in the second;
//! * the *data*: whatever device addresses a [`Segment`] names.
//!
//! The pages' device addresses need not be contiguous, and no code here
//! assumes they are.

#![no_std]
#![forbid(unsafe_code)]

pub mod command;
pub mod driver;
pub mod identify;
pub mod regs;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

pub use driver::{Ahci, CONTROL_PAGES, Error, Identity, Op, Segment};

/// The HBA's registers: its ABAR, BAR 5.
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

/// Whole pages of memory both this crate and the HBA reach.
pub trait Memory {
    /// The device address of each 4 KiB page, in order.
    fn pages(&self) -> &[u64];
    /// The byte at `offset`.
    fn read_u8(&self, offset: usize) -> u8;
    /// Write the byte at `offset`.
    fn write_u8(&mut self, offset: usize, value: u8);
    /// The little-endian `u16` at `offset`, a multiple of two, in one
    /// access: the HBA may be writing it.
    fn read_u16(&self, offset: usize) -> u16;
    /// Order every write made so far before whatever is written next, as
    /// the HBA sees them: called before `PxCI` is written.
    fn barrier(&self) {}
}
