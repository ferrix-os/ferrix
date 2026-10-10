//! A USB host as logic over registers and DMA memory: an EHCI or an xHCI
//! controller, the hubs behind it, and the keyboards, mice and disks behind
//! those.
//!
//! `src/user/system/native/drivers/usb/usbhid` runs this in a ring-3 process under devmgr, for the
//! STM32MP15 DK boards' USB host (`docs/INPUT.md` §7) and for a PC's xHCI
//! controllers (`docs/XHCI.md`). The process holds an
//! `IoMapping` of the controller's registers, its interrupt, a VMO pinned
//! with `PIN_COHERENT` for the memory the controller walks, and one control
//! channel to the kernel's input core per keyboard or mouse. None of those
//! exist in a unit test, so everything that decides a register value, a
//! descriptor or an event is here, written against [`Registers`], [`Dma`]
//! and [`Clock`], and tested against a model of each controller and of the
//! devices a DK board has on its bus:
//!
//! * [`host`]: what the bus asks of a controller, whichever it is;
//! * [`ehci`]: that controller -- its reset, its two schedules, control
//!   transfers, and interrupt pipes polled every frame;
//! * [`xhci`]: that one -- the hand-over from firmware, its rings, device
//!   slots, and control, interrupt and bulk transfers;
//! * [`storage`]: a disk over the bulk-only transport;
//! * [`usb`]: the standard requests and descriptors;
//! * [`hub`]: the hub class's requests and port status;
//! * [`hid`]: the boot protocol's reports, and the events and HELLO they make;
//! * [`bus`]: enumeration, hot plugging, and what the driver hands the core.
//!
//! # What is not here
//!
//! Only the boot protocol: a keyboard's or mouse's boot interface, whose
//! report layout the HID specification fixes, so no report descriptor is
//! parsed. Isochronous transfers, bulk transfers behind EHCI, a full- or
//! low-speed device on an EHCI root port -- which EHCI hands to a companion
//! controller -- and `SuperSpeed` hubs are not driven.
//!
//! # Waiting
//!
//! A control transfer takes a few frames, and a port reset fifty
//! milliseconds; enumeration waits for both in place, through
//! [`Clock::sleep_nanos`]. Interrupt pipes never wait: the controller
//! completes them in memory, and [`bus::Bus::on_interrupt`] reads what it
//! wrote.

#![no_std]
#![forbid(unsafe_code)]

pub mod bus;
pub mod ehci;
pub mod hid;
pub mod host;
pub mod hub;
pub mod report;
pub mod storage;
pub mod usb;
pub mod xhci;

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

/// A window of 32-bit device registers, by byte offset.
pub trait Registers {
    /// Read the register at `offset`.
    fn read32(&self, offset: u32) -> u32;
    /// Write the register at `offset`.
    fn write32(&mut self, offset: u32, value: u32);
}

/// The memory the controller walks: [`ehci::AREA_BYTES`] bytes, or what
/// [`xhci::area_bytes`] says, that the program and the controller see alike,
/// a page at a time wherever each page is.
pub trait Dma {
    /// How many bytes there are.
    fn len(&self) -> usize;
    /// Whether there are none.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Read the aligned word at `offset`.
    fn read32(&self, offset: usize) -> u32;
    /// Write the aligned word at `offset`.
    fn write32(&mut self, offset: usize, value: u32);
    /// Read the byte at `offset`.
    fn read8(&self, offset: usize) -> u8;
    /// Write the byte at `offset`.
    fn write8(&mut self, offset: usize, value: u8);
    /// The address the controller reaches the byte at `offset` by, if it
    /// has one below 4 GiB: EHCI's pointers are 32 bits.
    fn device_address(&self, offset: usize) -> Option<u32>;
    /// The address the controller reaches the byte at `offset` by, however
    /// high: for a controller whose pointers are 64 bits.
    fn wide_address(&self, offset: usize) -> Option<u64> {
        self.device_address(offset).map(u64::from)
    }
    /// Make every write before this visible to the controller before any
    /// write after it, to memory or to a register.
    fn barrier(&self);
}

/// Time, for the waits a bus needs.
pub trait Clock {
    /// Monotonic nanoseconds.
    fn now_nanos(&self) -> u64;
    /// Wait at least `nanos`.
    fn sleep_nanos(&mut self, nanos: u64);
}

/// What the driver is built from.
#[derive(Debug)]
pub struct Parts<R, D, C> {
    /// The controller's registers.
    pub registers: R,
    /// The memory it walks.
    pub memory: D,
    /// The time.
    pub clock: C,
}

/// A millisecond, in [`Clock`]'s units.
pub const MILLISECOND: u64 = 1_000_000;

/// Wait until `condition` holds, looking every `step` nanoseconds for at
/// most `limit`: whether it held.
fn wait_for<C: Clock>(
    clock: &mut C,
    limit: u64,
    step: u64,
    mut condition: impl FnMut() -> bool,
) -> bool {
    let until = clock.now_nanos().saturating_add(limit);
    loop {
        if condition() {
            return true;
        }
        if clock.now_nanos() >= until {
            return condition();
        }
        clock.sleep_nanos(step);
    }
}
