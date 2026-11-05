//! A planted kernel file, the fixture part of check-kernel-roots.py
//! (docs/microkernel/CRATE-SPLIT.md S0, ledger 7346 (5)). It is in no crate and
//! never built. The guard adds this directory as a kernel source root for one
//! run and requires every walker to walk this file, check-kernel-alloc-audit.py
//! to report the allocation below, and check-crate-layering.sh's rule 4 to refuse
//! the architecture conditional: a tool that read only src/kernel/src would see
//! none of it.

/// An allocation no audit lists.
pub fn planted_allocation() -> Box<u64> {
    Box::new(7)
}

/// An architecture conditional outside an arch directory.
#[cfg(target_arch = "x86_64")]
pub fn planted_architecture() {}
