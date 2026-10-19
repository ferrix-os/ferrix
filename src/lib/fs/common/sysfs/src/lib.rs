//! The text of sysfs, as pure functions of what the kernel and its services
//! know.
//!
//! sysfs is the device tree for the Linux ABI (`docs/SYSFS.md`): libdrm finds
//! a card's PCI identity in it, `lspci` lists functions from it, a C library
//! counts processors in it, and a service manager mounts cgroup2 inside it.
//! Each of those reads fixed formats written against what Linux prints -- a
//! vendor as `0x1af4`, a processor list as `0-3`, a capability bitmap as
//! words of a `long` -- and a format one character off is a program that
//! misreads the machine. So the formats are pinned here, against lines a
//! real Linux printed, where `cargo test` and the fuzzer reach them.
//!
//! Nothing here knows where a fact comes from. The kernel gathers them from
//! whoever owns each one -- enumeration, the cores the drivers publish into,
//! and `devmgr` -- and these functions only arrange them, as `ferrix-procfs`
//! does for `/proc` and `ferrix-cgroupfs` for the job tree.
//!
//! # What is here
//!
//! * [`attr`] -- the one-value files: identifiers in hexadecimal, numbers,
//!   device numbers, processor lists, hardware addresses.
//! * [`uevent`] -- the `KEY=value` lines every device's `uevent` file holds.
//! * [`pci`] -- a PCI function's `modalias` and `uevent`.
//! * [`input`] -- an input device's capability bitmaps, in words of the
//!   kernel's `long`, and its `uevent`.
//! * [`net`] -- an interface's `operstate`, `type` and `carrier`.
//! * [`drm`] -- a connector's name, `status`, `enabled` and `modes`.
//! * [`name`] -- the names directories are called by, and the name a write to
//!   `bind` or `unbind` gives.
//! * [`path`] -- a link's target, relative to the directory it is in.
//! * [`order`] -- the inode numbers and the listing cursors, from names.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod attr;
pub mod drm;
pub mod input;
pub mod name;
pub mod net;
pub mod order;
pub mod path;
pub mod pci;
mod text;
pub mod uevent;

#[cfg(test)]
mod tests;
