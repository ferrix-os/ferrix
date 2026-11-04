pub mod amdvi;
pub mod remapping;
pub mod vtd;
use crate::arch;
use crate::device::DeviceNode;
use crate::device::Location;
use crate::mm;
use crate::println;
use crate::sync::SpinLock;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::AtomicBool;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_acpi::dmar;
use ferrix_acpi::dmar::Structure;
use ferrix_acpi::iort;
use ferrix_bootinfo::BootView;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_fdt::EcamHost;
use ferrix_fdt::Fdt;
use ferrix_paging::MapError;
use ferrix_paging::MapFlags;
use ferrix_paging::vtd::rmrr;
use ferrix_paging::vtd::rmrr::Identity;
use ferrix_pci::Address;
use ferrix_pci::topology;
use ferrix_pci::topology::Bridge;
use ferrix_sync::Once;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unit {}
pub struct Counted(&'static str, usize);
impl core::fmt::Display for Counted {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fault {
    pub stream: u32,
    pub page: u64,
    pub write: bool,
    pub cause: Cause,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    Access,
    Event(u8),
    Lost,
    Overflow,
    Queue,
    Interrupt { reason: u8, index: u16 },
    AmdVi(u8),
    AmdViLost,
    AmdViFailed,
}
impl Fault {
    pub fn write_to(stream: u32, page: u64) -> Self {
        panic!("shim: iommu::Fault::write_to")
    }
}
impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DomainError {
    Empty,
    OutOfRange,
    Foreign,
    AlreadyPinned,
    Tables,
    Unit(&'static str),
    Reserved,
}
#[derive(Debug)]
pub struct Domain {}
#[derive(Debug)]
#[must_use = "unpin it, or leak its frames with it"]
pub struct Pinned {
    pub domain: u64,
    pub addresses: Vec<u64>,
}
impl Pinned {
    pub fn addresses(&self) -> &[u64] {
        panic!("shim: iommu::Pinned::addresses")
    }
    pub fn leak(self) {}
}
impl Domain {
    pub fn pin_budget(&self) -> &crate::object::pin::PinBudget {
        panic!("shim: iommu::Domain::pin_budget")
    }
    pub fn unpin_waiting(&self, pinned: Pinned, wait: Wait) -> Result<(), (DomainError, Pinned)> {
        panic!("shim: iommu::Domain::unpin_waiting")
    }
    #[doc(hidden)]
    #[must_use]
    pub const fn id(&self) -> u64 {
        0
    }
    pub fn untranslated() -> Self {
        panic!("shim: iommu::Domain::untranslated")
    }
    pub fn translated(&self) -> bool {
        false
    }
    pub fn resolve(&self, address: u64) -> Option<u64> {
        None
    }
    pub fn stream(&self) -> Option<u32> {
        None
    }
    pub fn take_fault(&self) -> Option<Fault> {
        None
    }
    pub fn provoke(&self, page: u64) {}
    pub fn pin(&self, frames: &[u64], flags: MapFlags) -> Result<Pinned, DomainError> {
        panic!("shim: iommu::Domain::pin")
    }
    pub fn unpin(&self, pinned: Pinned) -> Result<(), (DomainError, Pinned)> {
        panic!("shim: iommu::Domain::unpin")
    }
    pub fn queued(&self) -> bool {
        false
    }
}
pub fn domain_for(function: Address) -> Option<Domain> {
    None
}
pub fn counted(fault: Option<Fault>) -> Option<Fault> {
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    Status,
    Unwritten,
}
