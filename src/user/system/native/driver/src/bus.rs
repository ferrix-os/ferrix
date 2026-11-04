//! Buses ring-3 drivers own (`docs/DRIVER-MODEL.md` §5, step 4): child
//! devices a bus controller publishes, and the channel a child's driver
//! speaks to its parent over. A stub in this slice: devmgr's protocol does
//! not carry child devices yet, so a child is never started and its parent
//! bus never resolves.

use crate::model::{Errno, FromNode, Node, Requirement, Result};

/// A bus a child device is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BusKind {
    /// USB.
    Usb,
    /// I2C.
    I2c,
    /// SPI.
    Spi,
}

/// A child device a bus controller found.
#[derive(Debug)]
pub struct ChildDevice {
    /// Its bus.
    pub bus: BusKind,
}

/// A child device's place among its parent's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildId(pub u32);

/// An I2C device's channel to its controller's driver, its address fixed.
#[derive(Debug)]
pub struct I2cDevice(());

impl I2cDevice {
    /// Write `bytes`, then read `N`.
    ///
    /// # Errors
    ///
    /// `ENOSYS` until step 4.
    pub fn write_then_read<const N: usize>(&self, _bytes: &[u8]) -> Result<[u8; N]> {
        Err(Errno::ENOSYS)
    }
}

impl FromNode for I2cDevice {
    fn from_node(_node: &mut Node, _requirement: &Requirement<'_>) -> Result<Self> {
        // A generic node has no parent bus; a child device would.
        Err(Errno::ENOENT)
    }
}
