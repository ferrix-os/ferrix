//! The device tree bus: a peripheral the kernel found in a board's device
//! tree and published as a node (`DEVICE_TREE_BLOCKS`), its registers in
//! START's `common` block.
//!
//! Unlike [`crate::virtio`] this bus hands out no DMA memory: a tree device
//! here has no `dma`, so a driver on it pins nothing through this crate, and
//! its stop has nothing to free. [`Device::stopped`] says so in the type
//! the block subsystem asks for, once the driver has seen its controller
//! idle.

use core::marker::PhantomData;

use ferrix_blkring::control::Start;
use ferrix_rt::Kernel;
use ferrix_rt::native::device::Device as DeviceHandle;
use ferrix_rt::native::port::Port;

use crate::mmio;
use crate::start::Bind;
use crate::{Step, Stopped};

/// A device tree binding, as a type: its number in `DeviceInfo::device_id`.
pub trait Binding {
    /// The binding's number, `TREE_STM32_SDMMC` and the rest.
    const ID: u16;
}

/// A device tree node of binding `B`, its registers mapped.
pub struct Device<B: Binding> {
    handle: DeviceHandle<Kernel>,
    registers: mmio::Block,
    binding: PhantomData<B>,
}

impl<B: Binding> core::fmt::Debug for Device<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Device")
            .field("binding", &B::ID)
            .field("registers", &self.registers)
            .finish_non_exhaustive()
    }
}

impl<B: Binding> Bind for Device<B> {
    /// Map the node's registers. The interrupt is not claimed: a driver
    /// that waits on one claims it itself through [`Device::handle`].
    fn bind(
        handle: DeviceHandle<Kernel>,
        start: &Start,
        _port: &Port<Kernel>,
        _key: u64,
    ) -> Result<Self, Step> {
        if start.pci_device_id != B::ID {
            return Err(Step::Identity);
        }
        let registers = mmio::Block::map(&handle, &start.common)?;
        Ok(Device {
            handle,
            registers,
            binding: PhantomData,
        })
    }
}

impl<B: Binding> Device<B> {
    /// The node, for its clock and interrupt.
    #[must_use]
    pub fn handle(&self) -> &DeviceHandle<Kernel> {
        &self.handle
    }

    /// The registers.
    #[must_use]
    pub fn registers(&self) -> &mmio::Block {
        &self.registers
    }

    /// The registers, to write.
    pub fn registers_mut(&mut self) -> &mut mmio::Block {
        &mut self.registers
    }

    /// The node's clock rate, as `device_clock` says it without setting
    /// anything.
    ///
    /// # Errors
    ///
    /// [`Step::Device`] for a node with no clock the kernel reads.
    pub fn clock_hz(&self) -> Result<u64, Step> {
        self.handle
            .clock(1, false)
            .map(u64::from)
            .map_err(|_| Step::Device)
    }

    /// The proof a subsystem's `stop` returns, for a device that was given
    /// no memory: this bus pins none, so nothing it could write outlives
    /// the driver. The caller has seen its controller's data path idle.
    #[must_use]
    pub fn stopped(&self) -> Stopped {
        Stopped(())
    }
}
