//! The display engine's registers, mapped by the display side alone
//! (`docs/DESKTOP.md` §3.4): the first [`WINDOW`] bytes of BAR 0, uncached,
//! a mapping of its own beside the one the GT side holds, so that the card
//! needs nothing of the render node's state and moves with `card0` when
//! the driver splits (`docs/INTEL-GPU.md` §12.3).

use core::ptr;

use ferrix_native_abi::types::IoMappingSpec;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device, IoMapping};

/// How much of BAR 0 is mapped: every display register Alder Lake-N has
/// lies below 2 MiB (the transcoders and DDIs at `0x6xxxx`, the pipes at
/// `0x7xxxx`, the PCH's at `0xCxxxx`, the combo PHYs at `0x16xxxx`).
pub(crate) const WINDOW: u64 = 0x20_0000;

/// The display registers.
pub(crate) struct Regs {
    _mapping: IoMapping<Kernel>,
    base: usize,
}

impl Regs {
    /// Map the window at the start of BAR 0. `None`, and the reason, when
    /// there is no BAR 0 or the kernel refuses it.
    pub(crate) fn map(device: &Device<Kernel>, apertures: u32) -> Result<Self, &'static str> {
        let bar0 = (0..apertures as usize)
            .filter_map(|index| device.aperture(index).ok())
            .find(|aperture| aperture.bar == 0 && aperture.offset == 0 && aperture.len >= WINDOW)
            .ok_or("no BAR 0 to map the display registers from")?;
        let mapping = device
            .io_mapping(IoMappingSpec {
                phys: bar0.phys,
                len: WINDOW,
            })
            .map_err(|_| "the display registers' window was refused")?;
        let base = mapping
            .map(None)
            .map_err(|_| "the display registers' window could not be mapped")?;
        Ok(Self {
            _mapping: mapping,
            base,
        })
    }

    /// Where `offset` is, for an aligned four-byte access inside the window.
    fn at(&self, offset: u32) -> Option<usize> {
        (u64::from(offset) + 4 <= WINDOW && offset.is_multiple_of(4))
            .then(|| self.base + offset as usize)
    }
}

impl ferrix_intel_xelp::io::Mmio for Regs {
    fn read32(&mut self, offset: u32) -> u32 {
        let Some(address) = self.at(offset) else {
            return 0;
        };
        // SAFETY: inside the window mapped for this process and kept by
        // `self`, four-byte aligned.
        unsafe { register_read(address as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        let Some(address) = self.at(offset) else {
            return;
        };
        // SAFETY: as in `read32`.
        unsafe { register_write(address as *mut u32, value) }
    }
}

/// Read a register as one plain load, never folded into another access:
/// the main loop's `device_read` says why.
///
/// # Safety
///
/// `at` is mapped device memory, aligned.
#[inline(never)]
unsafe fn register_read(at: *const u32) -> u32 {
    // SAFETY: as the caller promises.
    unsafe { ptr::read_volatile(at) }
}

/// Write a register as one plain store.
///
/// # Safety
///
/// `at` is mapped, writable device memory, aligned.
#[inline(never)]
unsafe fn register_write(at: *mut u32, value: u32) {
    // SAFETY: as the caller promises.
    unsafe { ptr::write_volatile(at, value) }
}
