//! What every ring-3 driver process shares.
//!
//! A driver is a process devmgr starts on one device. Until this crate each
//! one carried the same plumbing in its own `main.rs`: reading START, mapping
//! the device's register blocks, pinning memory for it, the virtio transport,
//! the conversation with the kernel's interface core, the port loop, and the
//! exit statuses. What is left for a driver is the device itself.
//!
//! The layers, as Linux names them:
//!
//! * the driver core -- [`Bind`], [`Driver`], [`Step`];
//! * resources -- [`mmio::Block`], [`dma::Dma`];
//! * buses -- [`virtio`], whose [`virtio::Device`] is the transport every
//!   virtio device logic crate drives, and [`tree`], a board's peripheral
//!   the kernel published from its device tree;
//! * subsystems -- `input`, which speaks `inputctl` to the kernel's input
//!   core for any driver that implements `input::Device`, and `block`, which
//!   serves the block ring for any driver that implements `block::Device`.
//!
//! # Memory is freed only after a reset
//!
//! A device may write into its DMA memory until it is reset, so that memory
//! must not be freed before. Here it cannot be: a [`dma::Dma`] keeps its pin
//! when dropped, and the only way to free it is [`dma::Dma::free`], which
//! takes a [`Stopped`]. A `Stopped` is made only by the transport that saw
//! the device reset ([`virtio::Device::stopped`]) -- or by the tree bus,
//! which hands out no memory to free ([`tree::Device::stopped`]) -- and a
//! subsystem's `stop`
//! must return one or [`Stuck`]. So a driver cannot free memory early, and
//! cannot end without either proving the reset or leaving the memory pinned.

#![no_std]

pub mod dma;
pub mod mmio;
pub mod start;
pub mod tree;
pub mod virtio;

#[cfg(feature = "block")]
pub mod block;
#[cfg(feature = "input")]
pub mod input;

pub use start::{Bind, Started};

/// A driver: which device it binds to, and how it comes up on one.
pub trait Driver: Sized {
    /// The device it is started on; its type decides the match.
    type Device: Bind;

    /// Bring the driver up on `device`.
    ///
    /// # Errors
    ///
    /// The [`Step`] that failed, which becomes the exit status.
    fn probe(device: Self::Device) -> Result<Self, Step>;
}

/// Proof that the device was reset and can no longer touch its memory.
///
/// Its field is private: only this crate's transports make one, after they
/// saw the reset finish.
#[derive(Debug)]
pub struct Stopped(());

/// The device did not reset: its memory stays pinned, and the process ends
/// with it so.
#[derive(Debug)]
pub struct Stuck;

/// Where a run stopped, as the exit status devmgr reads: 0 is a clean stop.
///
/// The numbers are the ones every driver documented before this crate; a
/// subsystem may not reuse one for another meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device this driver does not drive.
    Identity = 2,
    /// A register block could not be mapped.
    Registers = 3,
    /// Memory could not be made, pinned or mapped.
    Memory = 4,
    /// The device would not come up, or would not describe itself.
    Device = 5,
    /// HELLO could not be sent, or READY did not come.
    Hello = 6,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
    /// The device broke the protocol.
    Faulted = 8,
    /// The device would not reset: its memory is kept.
    Wedged = 9,
    /// The control channel failed, or the core refused this driver.
    Control = 10,
}

impl Step {
    /// The exit status.
    #[must_use]
    pub const fn status(self) -> i32 {
        self as i32
    }
}
