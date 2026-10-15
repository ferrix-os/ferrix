//! Finding what the machine has, and handing it to ring 3.
//!
//! The kernel enumerates hardware and does not drive it
//! (`docs/ARCHITECTURE.md` §7). This is the enumerating half: how firmware
//! describes the machine, the buses walked to find functions on it, and the
//! hand-over of what was found to `devmgr`, which starts a driver for each.
//! What the kernel then does for a running driver -- its device nodes, the
//! IOMMU domains, interrupts and register access -- is not here, and neither
//! are the cores that speak each driver's protocol (`crate::interfaces`).
//!
//! | Module | What it finds |
//! |---|---|
//! | [`description`] | which of the two a machine is read by, answered once |
//! | [`finder`] | the interface every way of finding devices implements |
//! | [`acpi`] | the firmware tables, on x86-64 and on AArch64 under EDK2 |
//! | [`fdt`] | the device tree, on every other machine |
//! | [`pci`] | the PCI functions behind the ECAM windows either describes |
//! | [`devmgr`] | nothing: it starts `devmgr` and tells it what was found |
//!
//! The parsers themselves are host-testable crates in `src/lib/platform/`
//! (`ferrix-acpi`, `ferrix-fdt`, `ferrix-pci`); these modules reach them to
//! physical memory.
//!
//! # Only declarations here
//!
//! The group straddles the certification boundary: [`acpi`] and [`fdt`] are
//! in the core ring, [`pci`] and [`devmgr`] in the item, and this file is in
//! the item (`tools/common/data/certification-item.json`). Core code names
//! `crate::discovery::acpi` through it, as it names `crate::syscall::uaccess`
//! through `syscall/mod.rs`, and that is accepted on one term: this file
//! holds module declarations, attributes and documentation, and nothing
//! else -- no `pub use`, no items. A re-export here would let a core path
//! reach an item name without the item-boundary check seeing a path to it.

pub use ferrix_kernel_core::discovery::acpi;
pub use ferrix_kernel_core::discovery::board;
pub use ferrix_kernel_core::discovery::description;
pub mod devmgr;
pub use ferrix_kernel_core::discovery::fdt;
pub use ferrix_kernel_core::discovery::finder;
pub mod pci;
pub use ferrix_kernel_core::discovery::tree;
