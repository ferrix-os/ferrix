//! Memory a device reads and writes.

use core::mem::ManuallyDrop;
use core::ptr;
use core::sync::atomic::{Ordering, fence};

use ferrix_native_abi::rights::{Requested, Rights};
use ferrix_rt::Kernel;
use ferrix_rt::native::device::Device;
use ferrix_rt::native::handle::OwnedHandle;
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_virtio::QueueMemory;

use crate::{Step, Stopped};

/// A page, on every architecture a driver runs on.
pub const PAGE: usize = 4096;

/// The most pages one [`Dma`] holds: the block ring's data region is the
/// largest, at 128.
pub const MAX_PAGES: usize = 128;

/// Memory this process made, pinned read-write for a device and mapped.
///
/// Dropping it frees nothing: the pin stays until [`Dma::free`], which takes
/// the [`Stopped`] only a reset makes. A device that was not reset may still
/// write here, so memory it can reach is never handed back before.
pub struct Dma {
    pin: ManuallyDrop<Pin<Kernel>>,
    vmo: ManuallyDrop<Vmo<Kernel>>,
    base: usize,
    len: usize,
    addresses: [u64; MAX_PAGES],
    pages: usize,
}

impl core::fmt::Debug for Dma {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Dma")
            .field("len", &self.len)
            .field("device_pages", &self.device_pages())
            .finish_non_exhaustive()
    }
}

impl Dma {
    /// `pages` pages for `device`, pinned and mapped.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if they cannot be made, pinned or mapped, or if the
    /// kernel names fewer pages than asked.
    pub fn new(device: &Device<Kernel>, pages: usize) -> Result<Dma, Step> {
        if pages == 0 || pages > MAX_PAGES {
            return Err(Step::Memory);
        }
        let bytes = pages * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::ReadWrite)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; MAX_PAGES];
        let asked = raw.get_mut(..pages).ok_or(Step::Memory)?;
        let got = pin.addresses(asked).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != pages {
            return Err(Step::Memory);
        }
        let mut addresses = [0_u64; MAX_PAGES];
        for (slot, bytes) in addresses.iter_mut().zip(raw.iter().take(pages)) {
            *slot = device_address(*bytes);
        }
        let base = vmo
            .map(None, bytes, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Dma {
            pin: ManuallyDrop::new(pin),
            vmo: ManuallyDrop::new(vmo),
            base,
            len: bytes,
            addresses,
            pages,
        })
    }

    /// One device address per page, in order.
    #[must_use]
    pub fn device_pages(&self) -> &[u64] {
        self.addresses.get(..self.pages).unwrap_or_default()
    }

    /// A handle to the memory with exactly `rights`, for the kernel to map
    /// its side of it: the block ring's data region is one.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if the handle cannot be duplicated.
    pub fn share(&self, rights: Rights) -> Result<OwnedHandle<Kernel>, Step> {
        self.vmo
            .as_owned()
            .duplicate(Requested::Exactly(rights))
            .map_err(|_| Step::Memory)
    }

    /// Unpin and free the memory: the device was reset, so it can no longer
    /// write here.
    pub fn free(self, _reset: &Stopped) {
        let Dma { pin, vmo, .. } = self;
        drop(ManuallyDrop::into_inner(pin));
        drop(ManuallyDrop::into_inner(vmo));
    }

    /// The byte at `offset`.
    ///
    /// # Panics
    ///
    /// If `offset` is past the memory.
    #[must_use]
    pub fn read_u8(&self, offset: usize) -> u8 {
        assert!(offset < self.len, "a read inside the mapping");
        // SAFETY: a mapping the kernel made for this process that lives as
        // long as its VMO handle, which only `free` closes; the offset was
        // checked; volatile, since the other side is a device.
        unsafe { ptr::read_volatile((self.base + offset) as *const u8) }
    }

    /// Write the byte at `offset`.
    ///
    /// # Panics
    ///
    /// If `offset` is past the memory.
    pub fn write_u8(&mut self, offset: usize, value: u8) {
        assert!(offset < self.len, "a write inside the mapping");
        // SAFETY: as for `read_u8`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u8, value) }
    }

    /// The little-endian `u16` at `offset`, in one access: the other side may
    /// be writing it now, and two byte reads could take one byte from before
    /// its store and one from after.
    ///
    /// # Panics
    ///
    /// If the two bytes are not inside the memory, aligned.
    #[must_use]
    pub fn read_u16(&self, offset: usize) -> u16 {
        assert!(
            offset.checked_add(2).is_some_and(|end| end <= self.len),
            "a u16 inside the mapping"
        );
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `read_u8`, and the two bytes are one aligned `u16`.
        u16::from_le(unsafe { ptr::read_volatile(address as *const u16) })
    }

    /// Write the little-endian `u16` at `offset`, in one access: the other
    /// side may read it at any moment, and must never see one byte changed.
    ///
    /// # Panics
    ///
    /// If the two bytes are not inside the memory, aligned.
    pub fn write_u16(&mut self, offset: usize, value: u16) {
        assert!(
            offset.checked_add(2).is_some_and(|end| end <= self.len),
            "a u16 inside the mapping"
        );
        let address = self.base + offset;
        assert!(address.is_multiple_of(2), "a u16 on its own alignment");
        // SAFETY: as for `write_u8`, and the two bytes are one aligned `u16`.
        unsafe { ptr::write_volatile(address as *mut u16, value.to_le()) }
    }
}

/// The most pages one [`Run`] holds: the kernel's largest contiguous pin,
/// 4 MiB.
pub const MAX_RUN_PAGES: usize = 1024;

/// Memory this process made and pinned for a device as one run of device
/// addresses, and never mapped: what a device is lent for its own use, as
/// an NVMe controller's host memory buffer is.
///
/// Like a [`Dma`], dropping it frees nothing; only [`Run::free`] does, with
/// the proof of a reset.
pub struct Run {
    pin: ManuallyDrop<Pin<Kernel>>,
    vmo: ManuallyDrop<Vmo<Kernel>>,
    address: u64,
    pages: usize,
}

impl core::fmt::Debug for Run {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Run")
            .field("address", &self.address)
            .field("pages", &self.pages)
            .finish_non_exhaustive()
    }
}

impl Run {
    /// `pages` pages for `device`, pinned as one run.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if they cannot be made or pinned as one run, or if
    /// the addresses the kernel names are not one.
    pub fn new(device: &Device<Kernel>, pages: usize) -> Result<Run, Step> {
        if pages == 0 || pages > MAX_RUN_PAGES {
            return Err(Step::Memory);
        }
        let bytes = pages * PAGE;
        let vmo = vmo::create(Kernel, bytes).map_err(|_| Step::Memory)?;
        let pin = device
            .pin(&vmo, 0, bytes, PinAccess::Contiguous)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; MAX_RUN_PAGES];
        let asked = raw.get_mut(..pages).ok_or(Step::Memory)?;
        let got = pin.addresses(asked).map_err(|_| Step::Memory)?;
        let first = asked.first().copied().map(device_address);
        let one_run = asked.iter().enumerate().all(|(index, bytes)| {
            first.map(|first| first + (index * PAGE) as u64) == Some(device_address(*bytes))
        });
        let run = Run {
            pin: ManuallyDrop::new(pin),
            vmo: ManuallyDrop::new(vmo),
            address: first.unwrap_or(0),
            pages,
        };
        // A pin that is not what was asked stays pinned, as any other
        // memory a device might reach: the caller never learns its address.
        if !got.is_complete() || got.pages != pages || !one_run {
            return Err(Step::Memory);
        }
        Ok(run)
    }

    /// The device address of the first byte.
    #[must_use]
    pub fn address(&self) -> u64 {
        self.address
    }

    /// Pages in the run.
    #[must_use]
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Unpin and free the memory: the device was reset, so it can no longer
    /// write here.
    pub fn free(self, _reset: &Stopped) {
        let Run { pin, vmo, .. } = self;
        drop(ManuallyDrop::into_inner(pin));
        drop(ManuallyDrop::into_inner(vmo));
    }
}

// SAFETY: one VMO, pinned so the device sees the pages `device_pages` names
// and mapped so this process sees them, both until `free`, which only a
// reset allows. Every access is volatile and `barrier` is a full fence. Each
// `u16` is one access, as the trait requires of the ring's indices.
unsafe impl QueueMemory for Dma {
    fn read_u8(&self, offset: usize) -> u8 {
        Dma::read_u8(self, offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        Dma::write_u8(self, offset, value);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        Dma::read_u16(self, offset)
    }

    fn write_u16(&mut self, offset: usize, value: u16) {
        Dma::write_u16(self, offset, value);
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }
}

#[cfg(feature = "input")]
impl ferrix_virtio_input::DevicePages for Dma {
    fn device_pages(&self) -> &[u64] {
        Dma::device_pages(self)
    }
}

#[cfg(feature = "input")]
impl ferrix_virtio_input::EventArea for Dma {
    fn read_u8(&self, offset: usize) -> u8 {
        Dma::read_u8(self, offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        Dma::write_u8(self, offset, value);
    }
}

#[cfg(feature = "block")]
impl ferrix_virtio_blk::DevicePages for Dma {
    fn device_pages(&self) -> &[u64] {
        Dma::device_pages(self)
    }
}

#[cfg(feature = "block")]
impl ferrix_virtio_blk::RequestArea for Dma {
    fn read_u8(&self, offset: usize) -> u8 {
        Dma::read_u8(self, offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        Dma::write_u8(self, offset, value);
    }
}
