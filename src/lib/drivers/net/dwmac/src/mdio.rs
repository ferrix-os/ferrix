//! The MDIO bus through the core's own management interface, and a
//! clause-22 PHY on it: reset, advertise, auto-negotiate, and read what was
//! agreed.
//!
//! Only the standard registers are used, so any clause-22 copper PHY works
//! as the YT6801's integrated YT8531S does; a vendor's own status register
//! would say the same thing in a different place.

use crate::regs::{self, phy};
use crate::{Clock, Error, Registers};

/// How long one MDIO operation may take: a frame is 64 bits at a clock of
/// at least 1 MHz, so a millisecond is twenty times enough.
const MDIO_PATIENCE_NANOS: u64 = 1_000_000;

/// How long a PHY's reset may take: IEEE 802.3 22.2.4.1.1 allows 0.5 s.
const PHY_RESET_PATIENCE_NANOS: u64 = 500_000_000;

/// How long to wait between two looks at a busy bit.
const POLL_NANOS: u64 = 5_000;

/// How the MDIO bus is driven.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bus {
    /// The MDC clock range field: which divisor of the core's CSR clock
    /// keeps MDC under 2.5 MHz. 2 is a CSR clock of 20 to 35 MHz.
    pub clock_range: u8,
    /// Whether frames are sent without their 32-bit preamble.
    pub preamble_suppression: bool,
    /// The PHY's address on the bus.
    pub phy: u8,
}

/// A link's speed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    /// 10 Mb/s.
    Ten,
    /// 100 Mb/s.
    Hundred,
    /// 1000 Mb/s.
    Thousand,
}

/// What the PHY says of the link.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    /// No link, or auto-negotiation not finished.
    Down,
    /// Up, at this speed and duplex.
    Up {
        /// The speed agreed.
        speed: Speed,
        /// Whether full duplex was agreed.
        full_duplex: bool,
    },
}

/// The address register's value for one operation.
const fn command(bus: Bus, register: u8, operation: u32) -> u32 {
    let mut value = regs::MDIO_GB
        | operation
        | ((bus.clock_range as u32 & 0xF) << regs::MDIO_CR_SHIFT)
        | ((register as u32 & 0x1F) << regs::MDIO_RDA_SHIFT)
        | ((bus.phy as u32 & 0x1F) << regs::MDIO_PA_SHIFT);
    if bus.preamble_suppression {
        value |= regs::MDIO_PSE;
    }
    value
}

/// Wait for the busy bit to clear.
///
/// The clock is read *before* the busy bit, so the bus is given up on only
/// when a look taken after the deadline still finds it busy. A driver is a
/// process like any other, and on a loaded machine a pause of 5 us can come
/// back many milliseconds later: had the deadline been checked after the
/// look, an operation that finished during that pause would count as one
/// that never did. On the N150 box that was the link poll failing under a
/// full-speed transfer onto btrfs, and the driver leaving (2026-10-18).
fn settle<R: Registers, C: Clock>(registers: &R, clock: &mut C) -> Result<(), Error> {
    let deadline = clock.now_nanos().saturating_add(MDIO_PATIENCE_NANOS);
    loop {
        let late = clock.now_nanos() >= deadline;
        if registers.read(regs::MDIO_ADDRESS) & regs::MDIO_GB == 0 {
            return Ok(());
        }
        if late {
            return Err(Error::MdioTimeout);
        }
        clock.pause(POLL_NANOS);
    }
}

/// Read PHY register `register`.
///
/// # Errors
///
/// [`Error::MdioTimeout`] if the bus stays busy.
pub fn read<R: Registers, C: Clock>(
    registers: &mut R,
    clock: &mut C,
    bus: Bus,
    register: u8,
) -> Result<u16, Error> {
    settle(registers, clock)?;
    registers.write(
        regs::MDIO_ADDRESS,
        command(bus, register, regs::MDIO_GOC_READ),
    );
    settle(registers, clock)?;
    Ok((registers.read(regs::MDIO_DATA) & 0xFFFF) as u16)
}

/// Write `value` to PHY register `register`.
///
/// # Errors
///
/// [`Error::MdioTimeout`] if the bus stays busy.
pub fn write<R: Registers, C: Clock>(
    registers: &mut R,
    clock: &mut C,
    bus: Bus,
    register: u8,
    value: u16,
) -> Result<(), Error> {
    settle(registers, clock)?;
    registers.write(regs::MDIO_DATA, u32::from(value));
    registers.write(
        regs::MDIO_ADDRESS,
        command(bus, register, regs::MDIO_GOC_WRITE),
    );
    settle(registers, clock)
}

/// The PHY's identifier, high half first; all ones or all zeroes is no PHY
/// answering at that address.
///
/// # Errors
///
/// [`Error::MdioTimeout`], or [`Error::NoPhy`] with what was read.
pub fn identify<R: Registers, C: Clock>(
    registers: &mut R,
    clock: &mut C,
    bus: Bus,
) -> Result<u32, Error> {
    let high = read(registers, clock, bus, phy::PHYSID1)?;
    let low = read(registers, clock, bus, phy::PHYSID2)?;
    let id = (u32::from(high) << 16) | u32::from(low);
    if id == 0 || id == u32::MAX {
        return Err(Error::NoPhy(id));
    }
    Ok(id)
}

/// Reset the PHY, advertise 10, 100 and -- if `gigabit` -- 1000 Mb/s at
/// both duplexes, and start auto-negotiation.
///
/// # Errors
///
/// [`Error::MdioTimeout`], or [`Error::PhyResetTimeout`] if the PHY never
/// comes out of its reset.
pub fn start<R: Registers, C: Clock>(
    registers: &mut R,
    clock: &mut C,
    bus: Bus,
    gigabit: bool,
) -> Result<(), Error> {
    write(registers, clock, bus, phy::BMCR, phy::BMCR_RESET)?;
    let deadline = clock.now_nanos().saturating_add(PHY_RESET_PATIENCE_NANOS);
    // The clock before the look, as in `settle`.
    loop {
        let late = clock.now_nanos() >= deadline;
        if read(registers, clock, bus, phy::BMCR)? & phy::BMCR_RESET == 0 {
            break;
        }
        if late {
            return Err(Error::PhyResetTimeout);
        }
        clock.pause(POLL_NANOS * 200);
    }
    let advertise = phy::ADVERTISE_CSMA
        | phy::ADVERTISE_10HALF
        | phy::ADVERTISE_10FULL
        | phy::ADVERTISE_100HALF
        | phy::ADVERTISE_100FULL;
    write(registers, clock, bus, phy::ADVERTISE, advertise)?;
    let gigabit = if gigabit {
        phy::ADVERTISE_1000FULL | phy::ADVERTISE_1000HALF
    } else {
        0
    };
    let control = read(registers, clock, bus, phy::CTRL1000)?;
    let control = (control & !(phy::ADVERTISE_1000FULL | phy::ADVERTISE_1000HALF)) | gigabit;
    write(registers, clock, bus, phy::CTRL1000, control)?;
    write(
        registers,
        clock,
        bus,
        phy::BMCR,
        phy::BMCR_ANENABLE | phy::BMCR_ANRESTART,
    )
}

/// The link as the PHY sees it now.
///
/// # Errors
///
/// [`Error::MdioTimeout`].
pub fn link<R: Registers, C: Clock>(
    registers: &mut R,
    clock: &mut C,
    bus: Bus,
) -> Result<Link, Error> {
    // The link bit is latched low: the first read says whether it dropped
    // since the last, the second whether it is up now.
    let _ = read(registers, clock, bus, phy::BMSR)?;
    let status = read(registers, clock, bus, phy::BMSR)?;
    if status & phy::BMSR_LINK == 0 || status & phy::BMSR_ANEGCOMPLETE == 0 {
        return Ok(Link::Down);
    }
    let ours = read(registers, clock, bus, phy::CTRL1000)?;
    let theirs = read(registers, clock, bus, phy::STAT1000)?;
    let advertised = read(registers, clock, bus, phy::ADVERTISE)?;
    let partner = read(registers, clock, bus, phy::LPA)?;
    Ok(resolve(ours, theirs, advertised, partner))
}

/// The highest mode both ends advertised, as IEEE 802.3 Annex 28B.3 orders
/// them.
#[must_use]
pub const fn resolve(ours1000: u16, theirs1000: u16, advertised: u16, partner: u16) -> Link {
    if ours1000 & phy::ADVERTISE_1000FULL != 0 && theirs1000 & phy::LPA_1000FULL != 0 {
        return Link::Up {
            speed: Speed::Thousand,
            full_duplex: true,
        };
    }
    if ours1000 & phy::ADVERTISE_1000HALF != 0 && theirs1000 & phy::LPA_1000HALF != 0 {
        return Link::Up {
            speed: Speed::Thousand,
            full_duplex: false,
        };
    }
    let common = advertised & partner;
    if common & phy::ADVERTISE_100FULL != 0 {
        Link::Up {
            speed: Speed::Hundred,
            full_duplex: true,
        }
    } else if common & phy::ADVERTISE_100HALF != 0 {
        Link::Up {
            speed: Speed::Hundred,
            full_duplex: false,
        }
    } else if common & phy::ADVERTISE_10FULL != 0 {
        Link::Up {
            speed: Speed::Ten,
            full_duplex: true,
        }
    } else if common & phy::ADVERTISE_10HALF != 0 {
        Link::Up {
            speed: Speed::Ten,
            full_duplex: false,
        }
    } else {
        Link::Down
    }
}
