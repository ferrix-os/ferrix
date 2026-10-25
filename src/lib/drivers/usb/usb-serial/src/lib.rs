//! USB serial class drivers as logic over a claimed interface: FTDI's
//! UARTs and the USB-Blaster behind one (`docs/USB-SERIAL.md`).
//!
//! Everything here is written against [`Usb`], what a class driver can do
//! with the one interface it was given: control requests, a bulk OUT
//! stream that never waits, and a bulk IN stream read a chunk at a time.
//! `src/user/system/native/drivers/usb/usbserial` gives it over the
//! interface channel `usbhid` exports (`ferrix-usbif`); the tests in
//! `ferrix-usb-host` give it over the bus itself, against a model
//! controller and a model chip.
//!
//! * [`ftdi`]: the chips, their vendor requests, divisors and status bytes;
//! * [`port`]: one chip as a serial port, and as a JTAG cable;
//! * [`blaster`]: JTAG through the USB-Blaster's CPLD.

#![no_std]
#![forbid(unsafe_code)]

pub mod blaster;
pub mod ftdi;
pub mod mpsse;
pub mod port;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// The most one IN chunk holds: a stream's request.
pub const CHUNK_BYTES: usize = 512;

/// A control request's setup packet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Setup {
    /// `bmRequestType`.
    pub request_type: u8,
    /// `bRequest`.
    pub request: u8,
    /// `wValue`.
    pub value: u16,
    /// `wIndex`.
    pub index: u16,
    /// `wLength`.
    pub length: u16,
}

impl Setup {
    /// The packet's bytes, little-endian as the bus carries them.
    #[must_use]
    pub const fn bytes(&self) -> [u8; 8] {
        let value = self.value.to_le_bytes();
        let index = self.index.to_le_bytes();
        let length = self.length.to_le_bytes();
        [
            self.request_type,
            self.request,
            value[0],
            value[1],
            index[0],
            index[1],
            length[0],
            length[1],
        ]
    }
}

/// What a class driver can do with its interface.
pub trait Usb {
    /// What a transfer fails with.
    type Error: Copy + core::fmt::Debug;

    /// Run a control request with no data stage, or one into `data`.
    ///
    /// # Errors
    ///
    /// The transfer's, or a request the host refused.
    fn control(&mut self, setup: &Setup, data: &mut [u8]) -> Result<usize, Self::Error>;

    /// Queue as much of `bytes` for the OUT endpoint as there is room for,
    /// without waiting: how many.
    ///
    /// # Errors
    ///
    /// The interface is gone.
    fn send(&mut self, bytes: &[u8]) -> Result<usize, Self::Error>;

    /// Bytes queued that the device has not taken.
    fn in_flight(&self) -> usize;

    /// The next chunk the IN endpoint read, into `into`: its length. A
    /// chunk holds whole packets, the last perhaps short.
    fn take_chunk(&mut self, into: &mut [u8; CHUNK_BYTES]) -> Option<usize>;

    /// Wait a little for the device, taking what it sent meanwhile: what a
    /// caller that must have an answer -- a JTAG scan -- loops on.
    ///
    /// # Errors
    ///
    /// The interface is gone.
    fn pause(&mut self) -> Result<(), Self::Error>;

    /// Monotonic nanoseconds.
    fn now(&self) -> u64;
}
