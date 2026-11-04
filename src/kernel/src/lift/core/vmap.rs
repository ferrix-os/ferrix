use crate::fallible;
use crate::mm;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::Ordering;
use ferrix_bootinfo::KERNEL_VMAP_BASE;
use ferrix_bootinfo::KERNEL_VMAP_RESERVED;
use ferrix_bootinfo::KERNEL_VMAP_SIZE;
use ferrix_bootinfo::PAGE_SIZE;
use ferrix_paging::MapFlags;
use ferrix_sync::IrqSpinLock;
use ferrix_vma::AddressSpace;
use ferrix_vma::Backing;
use ferrix_vma::PageRange;
use ferrix_vma::VmaError;
use ferrix_vma::VmaFlags;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmapError {
    NotReady,
    BadLength(u64),
    NoAddressSpace(u64),
    OutOfMemory,
    MapFailed(ferrix_paging::MapError),
    NotAllocated(u64),
    KernelImage(u64),
}
impl core::fmt::Display for VmapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Anonymous,
    Device { phys: u64 },
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mapping {}
#[derive(Debug)]
pub struct Buffer {
    bytes: alloc::vec::Vec<u8>,
}
impl Buffer {
    pub fn zeroed(len: usize) -> Result<Buffer, VmapError> {
        let mut bytes = alloc::vec::Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(|_| VmapError::OutOfMemory)?;
        bytes.resize(len, 0);
        Ok(Buffer { bytes })
    }
    pub fn truncate(&mut self, len: usize) {
        self.bytes.truncate(len);
    }
}
impl core::ops::Deref for Buffer {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.bytes
    }
}
impl core::ops::DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}
pub fn reserve(len: u64, flags: VmaFlags, kind: Kind) -> Result<Mapping, VmapError> {
    panic!("shim: vmap::reserve")
}
pub fn map_device(phys: u64, len: u64) -> Result<u64, VmapError> {
    panic!("shim: vmap::map_device")
}
pub fn unmap_device(at: u64) -> Result<(), VmapError> {
    panic!("shim: vmap::unmap_device")
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub allocations: usize,
    pub bytes: u64,
}
pub fn usage() -> Usage {
    Usage::default()
}
