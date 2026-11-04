use super::amdvi;
use super::vtd;
use crate::iommu::Unit;
use crate::panic::catalog;
use crate::panic::fatal;
use crate::println;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::Ordering;
use ferrix_acpi::dmar;
use ferrix_acpi::dmar::Structure;
use ferrix_paging::vtd::remap;
use ferrix_paging::vtd::remap::Remap;
use ferrix_paging::vtd::remap::UnitState;
use ferrix_pci::Address;
use ferrix_sync::Once;
#[derive(Clone, Copy, Debug)]
pub enum Route {
    Compatibility,
}
