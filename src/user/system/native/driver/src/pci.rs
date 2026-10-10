//! The PCI bus: a PCI function that is not a virtio transport, as its
//! driver gets it -- its BARs to map, its interrupt, and memory to pin.
//!
//! START says only where the function is; what it is comes from
//! `device_info` on the handle, and where its registers are from
//! `device_aperture`. An aperture is a BAR, or the part of one either side
//! of the MSI-X table's pages, which the kernel keeps to itself
//! (`src/kernel/src/device.rs`): [`Device::map_bar`] maps the part that
//! starts the BAR, which is where a controller's registers are.
//!
//! The interrupt is the function's vector 0: its first MSI-X table entry,
//! or its one MSI message. The kernel programs either; a driver only binds
//! and acknowledges.
//!
//! Unlike [`crate::virtio`], this bus cannot see a function reset: how a
//! controller is stopped is its own protocol. So [`Device::stopped`] takes
//! the driver's word, as [`crate::tree::Device::stopped`] does, and the
//! driver gives it only after it saw its controller disabled.

use core::marker::PhantomData;

use ferrix_blkring::control::{Block as StartBlock, Start};
use ferrix_native_abi::types::DeviceInfo;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::{Device as DeviceHandle, Interrupt};
use ferrix_rt::native::port::Port;

use crate::dma::{Dma, Run};
use crate::mmio;
use crate::start::Bind;
use crate::{Step, Stopped};

/// A kind of PCI function, as a type: what its driver binds to.
pub trait Function {
    /// Whether `info` describes a function of this kind.
    fn matches(info: &DeviceInfo) -> bool;
}

/// Whether `info`'s class code is `base:sub:interface`, as a storage
/// controller's `01:08:02` or a USB host controller's `0C:03:30`.
#[must_use]
pub const fn class_is(info: &DeviceInfo, base: u8, sub: u8, interface: u8) -> bool {
    info.class & 0x00FF_FFFF == ((base as u32) << 16) | ((sub as u32) << 8) | interface as u32
}

/// A PCI function of kind `F`.
pub struct Device<F: Function> {
    handle: DeviceHandle<Kernel>,
    info: DeviceInfo,
    interrupt: Interrupt<Kernel>,
    kind: PhantomData<F>,
}

impl<F: Function> core::fmt::Debug for Device<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("location", &self.info.location)
            .field("vendor", &self.info.vendor_id)
            .field("device", &self.info.device_id)
            .finish_non_exhaustive()
    }
}

impl<F: Function> Bind for Device<F> {
    fn bind(
        handle: DeviceHandle<Kernel>,
        start: &Start,
        port: &Port<Kernel>,
        key: u64,
    ) -> Result<Self, Step> {
        let info = handle.info().map_err(|_| Step::Identity)?;
        if info.location != start.location || !F::matches(&info) {
            say(b"pci      a driver was started on a function that is not its kind\n");
            return Err(Step::Identity);
        }
        let vector = handle.interrupt(0).and_then(|interrupt| {
            interrupt.bind(port, key)?;
            Ok(interrupt)
        });
        let Ok(interrupt) = vector else {
            say(b"pci      the function's first MSI-X or MSI vector could not be claimed\n");
            return Err(Step::Events);
        };
        Ok(Device {
            handle,
            info,
            interrupt,
            kind: PhantomData,
        })
    }
}

impl<F: Function> Device<F> {
    /// The function as enumeration found it.
    #[must_use]
    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Whether the interrupt is an MSI-X table entry; otherwise it is the
    /// function's MSI message.
    #[must_use]
    pub fn msix(&self) -> bool {
        self.info.msix_table_size > 0
    }

    /// Map the start of BAR `bar`: the aperture that begins at its first
    /// byte, which ends where the BAR does or where the MSI-X table's pages
    /// begin.
    ///
    /// # Errors
    ///
    /// [`Step::Registers`] if the function has no such aperture, or the
    /// kernel will not map it.
    pub fn map_bar(&self, bar: u8) -> Result<mmio::Block, Step> {
        for index in 0..self.info.apertures as usize {
            let Ok(aperture) = self.handle.aperture(index) else {
                continue;
            };
            if aperture.bar != bar || aperture.offset != 0 {
                continue;
            }
            let length = u32::try_from(aperture.len).unwrap_or(u32::MAX);
            let mapped = mmio::Block::map(
                &self.handle,
                &StartBlock {
                    phys: aperture.phys,
                    offset: 0,
                    length,
                },
            );
            if mapped.is_err() {
                say(b"pci      the kernel would not map the function's BAR\n");
            }
            return mapped;
        }
        say(
            b"pci      the function's BAR is not published: its memory decoding is off, \
              or the kernel withheld it\n",
        );
        Err(Step::Registers)
    }

    /// `pages` pages of memory this function reads and writes, mapped.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if they cannot be made, pinned or mapped.
    pub fn dma(&self, pages: usize) -> Result<Dma, Step> {
        Dma::new(&self.handle, pages)
    }

    /// `pages` pages of memory this function reads and writes as one run
    /// of addresses, not mapped here: memory lent to the device for its
    /// own use.
    ///
    /// # Errors
    ///
    /// [`Step::Memory`] if no such run can be had.
    pub fn run(&self, pages: usize) -> Result<Run, Step> {
        Run::new(&self.handle, pages)
    }

    /// Re-arm the interrupt after a delivery.
    pub fn acknowledge_interrupt(&mut self) {
        let _ = self.interrupt.ack();
    }

    /// The proof a subsystem's `stop` returns. The caller has seen its
    /// controller disabled -- for NVMe, `CSTS.RDY` clear after `CC.EN` was
    /// cleared -- so the function reads and writes no memory any more.
    #[must_use]
    pub fn stopped(&self) -> Stopped {
        Stopped(())
    }
}

/// A line on standard error, which a native process has open on the
/// console: why a driver stopped before it could say anything itself.
fn say(line: &[u8]) {
    let _ = ferrix_rt::linux::write(2, line);
}
