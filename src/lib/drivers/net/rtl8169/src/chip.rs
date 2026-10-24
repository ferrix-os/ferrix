//! The 8168's indirect address spaces, and the chip-specific steps of
//! bringing an RTL8111H up and taking it down.
//!
//! Four buses hang off the register window, each a command word written
//! and, for most, a flag waited on:
//!
//! * **ERI**, the extended registers ([`eri_read`], [`eri_write`]): FIFO
//!   sizes, pause thresholds, the packet filter's reset, EEE, and the
//!   permanent station address.
//! * **MAC OCP**, the MAC's on-chip-processor bus ([`mac_ocp_read`],
//!   [`mac_ocp_write`]): power-management and PCIe tuning words. No flag:
//!   a read answers in the same register.
//! * **PHY OCP**, the gigabit PHY's registers by address ([`phy_read`],
//!   [`phy_write`]): page 0's clause-22 registers are at
//!   [`regs::phy::STANDARD_BASE`] onward, and register `r` (16 or above)
//!   of page `p` is at `p << 4` plus `(r - 16) * 2`. Addressing a page this
//!   way leaves no page selected, so a failed sequence cannot leave the PHY
//!   on the wrong page for the next one.
//! * **EPHY**, the PCIe PHY ([`ephy_read`], [`ephy_write`]).
//!
//! Every wait has a deadline. A wait looks at the flag a few times before
//! it sleeps, because the buses answer in microseconds and a sleep in a
//! ring-3 process may be a millisecond, and it takes the clock before each
//! look, so a flag that was set while the process was off the processor is
//! not one that never was.

use crate::regs::{self, phy};
use crate::{Clock, Error, Registers, Timeout};

/// Looks at a flag before the first sleep.
const SPINS: u32 = 64;

/// The sleep between later looks.
const POLL_NANOS: u64 = 20_000;

/// How long an ERI, OCP or EPHY access may take: Linux allows 10 ms for
/// ERI, a quarter of a millisecond for the PHY's bus and 1 ms for the
/// EPHY's; a loaded machine gets more here.
const ACCESS_PATIENCE_NANOS: u64 = 20_000_000;

/// How long the chip's reset may take: Linux allows 10 ms.
const RESET_PATIENCE_NANOS: u64 = 100_000_000;

/// How long the PHY's software reset may take: Linux allows 600 ms.
const PHY_RESET_PATIENCE_NANOS: u64 = 1_000_000_000;

/// How long the FIFOs may take to empty or the link list to be ready:
/// Linux allows about 4 ms each.
const FIFO_PATIENCE_NANOS: u64 = 20_000_000;

/// Wait for `done` to hold, at most `patience` nanoseconds.
fn wait<R: Registers, C: Clock>(
    regs: &R,
    clock: &mut C,
    patience: u64,
    mut done: impl FnMut(&R) -> bool,
) -> bool {
    let deadline = clock.now_nanos().saturating_add(patience);
    let mut spins = 0;
    loop {
        let late = clock.now_nanos() >= deadline;
        if done(regs) {
            return true;
        }
        if late {
            return false;
        }
        if spins < SPINS {
            spins += 1;
            core::hint::spin_loop();
        } else {
            clock.pause(POLL_NANOS);
        }
    }
}

/// Wait for an access register's flag to read `set`.
fn wait_flag<R: Registers, C: Clock>(
    regs: &R,
    clock: &mut C,
    register: u32,
    set: bool,
    bus: Timeout,
) -> Result<(), Error> {
    if wait(regs, clock, ACCESS_PATIENCE_NANOS, |regs| {
        (regs.read32(register) & regs::ACCESS_FLAG != 0) == set
    }) {
        Ok(())
    } else {
        Err(Error::Timeout(bus))
    }
}

/// The ERI word at `address`, a multiple of four.
///
/// # Errors
///
/// [`Error::Timeout`] if the read never finished.
pub fn eri_read<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    address: u16,
) -> Result<u32, Error> {
    let command = (regs::ERI_MASK_ALL << regs::ERI_MASK_SHIFT) | u32::from(address & 0xFFC);
    regs.write32(regs::ERIAR, command);
    wait_flag(regs, clock, regs::ERIAR, true, Timeout::Eri)?;
    Ok(regs.read32(regs::ERIDR))
}

/// Write the bytes `mask` names of the ERI word at `address`.
///
/// # Errors
///
/// [`Error::Timeout`] if the write never finished.
pub fn eri_write<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    address: u16,
    mask: u32,
    value: u32,
) -> Result<(), Error> {
    regs.write32(regs::ERIDR, value);
    regs.write32(
        regs::ERIAR,
        regs::ACCESS_FLAG | ((mask & 0xF) << regs::ERI_MASK_SHIFT) | u32::from(address & 0xFFC),
    );
    wait_flag(regs, clock, regs::ERIAR, false, Timeout::Eri)
}

/// Clear `clear` and set `set` in the ERI word at `address`.
///
/// # Errors
///
/// As [`eri_read`] and [`eri_write`].
pub fn eri_modify<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    address: u16,
    clear: u32,
    set: u32,
) -> Result<(), Error> {
    let value = eri_read(regs, clock, address)?;
    eri_write(
        regs,
        clock,
        address,
        regs::ERI_MASK_ALL,
        (value & !clear) | set,
    )
}

/// The OCP command word for `address`, which must be even.
fn ocp_address(address: u16) -> Result<u32, Error> {
    if address & 1 != 0 {
        return Err(Error::BadOptions);
    }
    Ok(u32::from(address) << regs::OCP_ADDRESS_SHIFT)
}

/// The MAC OCP word at `address`.
///
/// # Errors
///
/// [`Error::BadOptions`] for an odd address.
pub fn mac_ocp_read<R: Registers>(regs: &mut R, address: u16) -> Result<u16, Error> {
    regs.write32(regs::MAC_OCP, ocp_address(address)?);
    Ok(regs.read32(regs::MAC_OCP) as u16)
}

/// Write the MAC OCP word at `address`.
///
/// # Errors
///
/// [`Error::BadOptions`] for an odd address.
pub fn mac_ocp_write<R: Registers>(regs: &mut R, address: u16, value: u16) -> Result<(), Error> {
    regs.write32(
        regs::MAC_OCP,
        regs::ACCESS_FLAG | ocp_address(address)? | u32::from(value),
    );
    Ok(())
}

/// Clear `clear` and set `set` in the MAC OCP word at `address`.
///
/// # Errors
///
/// [`Error::BadOptions`] for an odd address.
pub fn mac_ocp_modify<R: Registers>(
    regs: &mut R,
    address: u16,
    clear: u16,
    set: u16,
) -> Result<(), Error> {
    let value = mac_ocp_read(regs, address)?;
    mac_ocp_write(regs, address, (value & !clear) | set)
}

/// Where register `register` of PHY page `page` is on the PHY's OCP bus.
///
/// Page 0 is the clause-22 set at [`phy::STANDARD_BASE`]; on any other page
/// only registers 16 to 31 exist.
///
/// # Errors
///
/// [`Error::BadOptions`] for a register below 16 on a page other than 0,
/// or one above 31.
pub const fn phy_address(page: u16, register: u8) -> Result<u16, Error> {
    if register > 31 {
        return Err(Error::BadOptions);
    }
    if page == 0 {
        return Ok(phy::STANDARD_BASE + 2 * register as u16);
    }
    if register < 16 || page > 0x0FFF {
        return Err(Error::BadOptions);
    }
    Ok((page << 4) + 2 * (register as u16 - 16))
}

/// The PHY's register `register` on page `page`.
///
/// # Errors
///
/// [`Error::Timeout`] if the read never finished, or as [`phy_address`].
pub fn phy_read<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    page: u16,
    register: u8,
) -> Result<u16, Error> {
    let command = ocp_address(phy_address(page, register)?)?;
    regs.write32(regs::PHY_OCP, command);
    wait_flag(regs, clock, regs::PHY_OCP, true, Timeout::Phy)?;
    Ok(regs.read32(regs::PHY_OCP) as u16)
}

/// Write the PHY's register `register` on page `page`.
///
/// # Errors
///
/// As [`phy_read`].
pub fn phy_write<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    page: u16,
    register: u8,
    value: u16,
) -> Result<(), Error> {
    let command = ocp_address(phy_address(page, register)?)?;
    regs.write32(
        regs::PHY_OCP,
        regs::ACCESS_FLAG | command | u32::from(value),
    );
    wait_flag(regs, clock, regs::PHY_OCP, false, Timeout::Phy)
}

/// Clear `clear` and set `set` in the PHY's register `register` on page
/// `page`.
///
/// # Errors
///
/// As [`phy_read`].
pub fn phy_modify<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    page: u16,
    register: u8,
    clear: u16,
    set: u16,
) -> Result<(), Error> {
    let value = phy_read(regs, clock, page, register)?;
    phy_write(regs, clock, page, register, (value & !clear) | set)
}

/// Clear `clear` and set `set` in the PHY's parameter `parameter`: an
/// address written to page 0xA43's register 0x13 and the word behind it in
/// its register 0x14.
fn phy_parameter<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    parameter: u16,
    clear: u16,
    set: u16,
) -> Result<(), Error> {
    phy_write(regs, clock, 0x0A43, 0x13, parameter)?;
    phy_modify(regs, clock, 0x0A43, 0x14, clear, set)
}

/// The PCIe PHY's register `register`.
///
/// # Errors
///
/// [`Error::Timeout`] if the read never finished.
pub fn ephy_read<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    register: u8,
) -> Result<u16, Error> {
    regs.write32(
        regs::EPHYAR,
        u32::from(register & 0x1F) << regs::EPHY_REG_SHIFT,
    );
    wait_flag(regs, clock, regs::EPHYAR, true, Timeout::Ephy)?;
    Ok(regs.read32(regs::EPHYAR) as u16)
}

/// Write the PCIe PHY's register `register`.
///
/// # Errors
///
/// [`Error::Timeout`] if the write never finished.
pub fn ephy_write<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    register: u8,
    value: u16,
) -> Result<(), Error> {
    regs.write32(
        regs::EPHYAR,
        regs::ACCESS_FLAG | (u32::from(register & 0x1F) << regs::EPHY_REG_SHIFT) | u32::from(value),
    );
    wait_flag(regs, clock, regs::EPHYAR, false, Timeout::Ephy)?;
    // Linux lets 10 us pass after every EPHY write.
    clock.pause(10_000);
    Ok(())
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// A chip this driver drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Chip {
    /// The RTL8168H / RTL8111H: XID 0x541, Linux's `RTL_GIGA_MAC_VER_46`.
    Rtl8168h,
    /// The RTL8168M, which Realtek handles as an RTL8168H: XID 0x6C0.
    Rtl8168m,
}

/// The chip [`regs::TX_CONFIG`]'s identity bits name, or why not.
///
/// # Errors
///
/// [`Error::Absent`] when the register reads all ones (the function is not
/// answering), [`Error::Unsupported`] with the XID for any other chip.
pub fn identify(tx_config: u32) -> Result<(Chip, u16), Error> {
    if tx_config == u32::MAX {
        return Err(Error::Absent);
    }
    let xid = ((tx_config >> regs::XID_SHIFT) & regs::XID_MASK) as u16;
    match xid & 0x7CF {
        0x541 => Ok((Chip::Rtl8168h, xid)),
        0x6C0 => Ok((Chip::Rtl8168m, xid)),
        _ => Err(Error::Unsupported(xid)),
    }
}

// ---------------------------------------------------------------------------
// Bring-up and take-down
// ---------------------------------------------------------------------------

/// Unlock the configuration registers.
fn unlock<R: Registers>(regs: &mut R) {
    regs.write8(regs::CFG9346, regs::CFG_UNLOCK);
}

/// Lock them again.
fn lock<R: Registers>(regs: &mut R) {
    regs.write8(regs::CFG9346, regs::CFG_LOCK);
}

/// Gate the receive path off from the MAC and wait for both FIFOs to
/// empty, as the 8168G family wants before a reset. A FIFO that never
/// empties is not an error: the reset after this clears it.
pub fn quiesce<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) {
    let misc = regs.read32(regs::MISC);
    regs.write32(regs::MISC, misc | regs::MISC_RXDV_GATE);
    clock.pause(2_000_000);
    let _ = wait(regs, clock, FIFO_PATIENCE_NANOS, |regs| {
        regs.read32(regs::TX_CONFIG) & regs::TX_EMPTY != 0
    });
    let empty = regs::MCU_TX_EMPTY | regs::MCU_RX_EMPTY;
    let _ = wait(regs, clock, FIFO_PATIENCE_NANOS, |regs| {
        regs.read8(regs::MCU) & empty == empty
    });
}

/// Take the chip from firmware: the receive path gated, both directions
/// off, the out-of-band firmware's claim cleared, and the shared FIFO's
/// link list rebuilt (the 8168G family's "hardware init").
///
/// # Errors
///
/// [`Error::Timeout`] if the link list never became ready.
pub fn take_over<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<(), Error> {
    quiesce(regs, clock);
    let command = regs.read8(regs::CHIP_CMD);
    regs.write8(
        regs::CHIP_CMD,
        command & !(regs::CMD_TX_ENABLE | regs::CMD_RX_ENABLE),
    );
    clock.pause(1_000_000);
    let mcu = regs.read8(regs::MCU);
    regs.write8(regs::MCU, mcu & !regs::MCU_NOW_IS_OOB);
    let ready = |regs: &R| regs.read8(regs::MCU) & regs::MCU_LINK_LIST_READY != 0;
    mac_ocp_modify(regs, 0xE8DE, 1 << 14, 0)?;
    if !wait(regs, clock, FIFO_PATIENCE_NANOS, ready) {
        return Err(Error::Timeout(Timeout::LinkList));
    }
    mac_ocp_modify(regs, 0xE8DE, 0, 1 << 15)?;
    if !wait(regs, clock, FIFO_PATIENCE_NANOS, ready) {
        return Err(Error::Timeout(Timeout::LinkList));
    }
    Ok(())
}

/// Reset the chip and wait for the reset to finish.
///
/// # Errors
///
/// [`Error::Timeout`] if it never did.
pub fn reset<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<(), Error> {
    regs.write8(regs::CHIP_CMD, regs::CMD_RESET);
    if wait(regs, clock, RESET_PATIENCE_NANOS, |regs| {
        regs.read8(regs::CHIP_CMD) & regs::CMD_RESET == 0
    }) {
        Ok(())
    } else {
        Err(Error::Timeout(Timeout::Reset))
    }
}

/// The permanent station address, from the ERI words 0xE0 and 0xE4, which
/// the chip loads from its eFuse or EEPROM at power-on.
///
/// # Errors
///
/// As [`eri_read`].
pub fn permanent_mac<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
) -> Result<[u8; 6], Error> {
    let low = eri_read(regs, clock, 0xE0)?.to_le_bytes();
    let high = eri_read(regs, clock, 0xE4)?.to_le_bytes();
    Ok([low[0], low[1], low[2], low[3], high[0], high[1]])
}

/// The station address the receive filter holds now.
pub fn current_mac<R: Registers>(regs: &R) -> [u8; 6] {
    let low = regs.read32(regs::MAC0).to_le_bytes();
    let high = regs.read32(regs::MAC4).to_le_bytes();
    [low[0], low[1], low[2], low[3], high[0], high[1]]
}

/// Program the station address the receive filter matches.
pub fn set_mac<R: Registers>(regs: &mut R, mac: [u8; 6]) {
    let [a, b, c, d, e, f] = mac;
    unlock(regs);
    regs.write32(regs::MAC4, u32::from(u16::from_le_bytes([e, f])));
    let _ = regs.read8(regs::CHIP_CMD);
    regs.write32(regs::MAC0, u32::from_le_bytes([a, b, c, d]));
    let _ = regs.read8(regs::CHIP_CMD);
    lock(regs);
}

/// The PHY's identifier, from its registers 2 and 3.
///
/// # Errors
///
/// As [`phy_read`].
pub fn phy_id<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<u32, Error> {
    let high = phy_read(regs, clock, 0, phy::PHYSID1)?;
    let low = phy_read(regs, clock, 0, phy::PHYSID2)?;
    Ok((u32::from(high) << 16) | u32::from(low))
}

/// The ADC bias current offset the chip's eFuse holds for its PHY, as the
/// 8168H's PHY set-up writes it to page 0xBCF's register 0x16; `0xFFFF`
/// means none.
fn adc_bias_offset<R: Registers>(regs: &mut R) -> Result<u16, Error> {
    mac_ocp_write(regs, 0xDD02, 0x807D)?;
    let high = mac_ocp_read(regs, 0xDD02)?;
    let data = mac_ocp_read(regs, 0xDD00)?;
    let mut offset = ((data >> 1) & 0x7FF8) | (data & 0x0007);
    if high & (1 << 7) != 0 {
        offset |= 1 << 15;
    }
    Ok(offset)
}

/// The RTL8111H's PHY set-up, without Realtek's PHY patch firmware
/// (`rtl_nic/rtl8168h-2.fw`, an EEE patch this driver has no use for with
/// EEE off): channel estimation and retune parameters, the 10 Mb/s
/// amplitude, the ADC bias from eFuse, the transmit filter's corner, and
/// the PHY's own power saving (PFM, 10 Mb/s PLL off, ALDPS) off.
///
/// Then **EEE off**: nothing advertised (page 0xA5D, register 0x10). EEE's
/// low-power idle on the 8168 family has a long history of link drops and
/// stalls with some partners, and the driver gains nothing from it.
///
/// Then a software reset, the advertisement -- 10/100 both duplexes, 1000
/// full if `gigabit`, symmetric pause -- and auto-negotiation restarted.
///
/// # Errors
///
/// As [`phy_read`], or [`Error::Timeout`] if the PHY never left its reset.
pub fn phy_setup<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    gigabit: bool,
) -> Result<(), Error> {
    // Out of power-down first: its registers answer either way, but a
    // powered-down PHY ignores what is asked of its analogue side.
    phy_modify(regs, clock, 0, phy::BMCR, phy::BMCR_POWER_DOWN, 0)?;
    phy_parameter(regs, clock, 0x808A, 0x003F, 0x000A)?;
    phy_parameter(regs, clock, 0x0811, 0, 0x0800)?;
    phy_modify(regs, clock, 0x0A42, 0x16, 0, 0x0002)?;
    phy_modify(regs, clock, 0x0A44, 0x11, 0, 1 << 11)?;
    let bias = adc_bias_offset(regs)?;
    if bias != 0xFFFF {
        phy_write(regs, clock, 0x0BCF, 0x16, bias)?;
    }
    let level = phy_read(regs, clock, 0x0BCD, 0x16)? & 0x000F;
    let corner = level.saturating_sub(3);
    phy_write(regs, clock, 0x0BCD, 0x17, corner * 0x1111)?;
    phy_modify(regs, clock, 0x0A44, 0x11, 1 << 7, 0)?;
    phy_modify(regs, clock, 0x0A43, 0x10, 1 << 0, 0)?;
    phy_modify(regs, clock, 0x0A43, 0x10, 1 << 2, 0)?;
    phy_write(regs, clock, 0x0A5D, 0x10, 0)?;

    phy_write(regs, clock, 0, phy::BMCR, phy::BMCR_RESET)?;
    let deadline = clock.now_nanos().saturating_add(PHY_RESET_PATIENCE_NANOS);
    loop {
        let late = clock.now_nanos() >= deadline;
        if phy_read(regs, clock, 0, phy::BMCR)? & phy::BMCR_RESET == 0 {
            break;
        }
        if late {
            return Err(Error::Timeout(Timeout::PhyReset));
        }
        clock.pause(1_000_000);
    }
    phy_write(regs, clock, 0, phy::ADVERTISE, phy::ADVERTISE_ALL)?;
    let thousand = if gigabit { phy::ADVERTISE_1000_FULL } else { 0 };
    phy_modify(
        regs,
        clock,
        0,
        phy::CTRL1000,
        phy::ADVERTISE_1000_FULL | 0x0100,
        thousand,
    )?;
    phy_write(
        regs,
        clock,
        0,
        phy::BMCR,
        phy::BMCR_ANENABLE | phy::BMCR_ANRESTART,
    )
}

/// The EPHY table for the RTL8111H: register, bits to clear, bits to set.
const EPHY_8168H: [(u8, u16, u16); 6] = [
    (0x1E, 0x0800, 0x0001),
    (0x1D, 0x0000, 0x0800),
    (0x05, 0xFFFF, 0x2089),
    (0x06, 0xFFFF, 0x5881),
    (0x04, 0xFFFF, 0x854A),
    (0x01, 0xFFFF, 0x068B),
];

/// The RTL8111H's start: its PCIe PHY tuning, FIFO sizes and pause
/// thresholds, the packet filter reset, the chip's own ASPM and `CLKREQ#`
/// off, EEE off in the MAC, its power-saving modes off, and the timer and
/// tuning words of its MAC OCP space. Everything before the rings are
/// given; the configuration registers are unlocked throughout.
///
/// Two things Linux does here are not done, because both are writes to
/// the function's configuration space, which a Ferrix driver may not make:
/// the ASPM entry latency at offset 0x70F, and the PCIe maximum read
/// request. Firmware's values stand.
///
/// # Errors
///
/// As [`eri_read`], [`ephy_read`] and [`phy_read`].
pub fn start_8168h<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<(), Error> {
    // The chip's own ASPM and CLKREQ# off, before the EPHY is touched; the
    // link's ASPM control stays as firmware set it.
    mac_ocp_modify(regs, 0xE092, 0x00FF, 0)?;
    let config2 = regs.read8(regs::CONFIG2);
    regs.write8(regs::CONFIG2, config2 & !regs::CONFIG2_CLKREQ);
    let config5 = regs.read8(regs::CONFIG5);
    regs.write8(regs::CONFIG5, config5 & !regs::CONFIG5_ASPM);

    regs.write8(regs::MAX_TX_PACKET, regs::EARLY_SIZE);
    for (register, clear, set) in EPHY_8168H {
        let value = ephy_read(regs, clock, register)?;
        ephy_write(regs, clock, register, (value & !clear) | set)?;
    }
    eri_write(regs, clock, 0xC8, regs::ERI_MASK_ALL, (0x08 << 16) | 0x02)?;
    eri_write(regs, clock, 0xE8, regs::ERI_MASK_ALL, (0x10 << 16) | 0x06)?;
    eri_write(regs, clock, 0xCC, regs::ERI_MASK_LOW1, 0x38)?;
    eri_write(regs, clock, 0xD0, regs::ERI_MASK_LOW1, 0x48)?;
    eri_modify(regs, clock, 0xDC, 1 << 0, 0)?;
    eri_modify(regs, clock, 0xDC, 0, 1 << 0)?;
    eri_modify(regs, clock, 0xDC, 0, 0x001C)?;
    eri_write(regs, clock, 0x5F0, regs::ERI_MASK_LOW2, 0x4F87)?;
    let misc = regs.read32(regs::MISC);
    regs.write32(regs::MISC, misc & !regs::MISC_RXDV_GATE);
    eri_write(regs, clock, 0xC0, regs::ERI_MASK_LOW2, 0)?;
    eri_write(regs, clock, 0xB8, regs::ERI_MASK_LOW2, 0)?;
    // EEE off in the MAC (Linux sets these two bits).
    eri_modify(regs, clock, 0x1B0, 0x0003, 0)?;
    let dllpr = regs.read8(regs::DLLPR);
    regs.write8(
        regs::DLLPR,
        dllpr & !(regs::DLLPR_PFM | regs::DLLPR_TX_10M_PS),
    );
    let misc1 = regs.read8(regs::MISC_1);
    regs.write8(regs::MISC_1, misc1 & !regs::MISC_1_PFM_D3COLD);
    eri_modify(regs, clock, 0x1B0, 1 << 12, 0)?;
    let config3 = regs.read8(regs::CONFIG3);
    regs.write8(regs::CONFIG3, config3 & !regs::CONFIG3_READY_L23);

    // The 1 ms software counter, from the PHY's sawtooth count.
    let saw = phy_read(regs, clock, 0x0C42, 0x13)? & 0x3FFF;
    if saw > 0 {
        let count = (16_000_000 / u32::from(saw)) as u16 & 0x0FFF;
        mac_ocp_modify(regs, 0xD412, 0x0FFF, count)?;
    }
    mac_ocp_modify(regs, 0xE056, 0x00F0, 0)?;
    mac_ocp_modify(regs, 0xE052, 0x6000, 0x8008)?;
    mac_ocp_modify(regs, 0xE0D6, 0x01FF, 0x017F)?;
    mac_ocp_modify(regs, 0xD420, 0x0FFF, 0x047F)?;
    mac_ocp_write(regs, 0xE63E, 0x0001)?;
    mac_ocp_write(regs, 0xE63E, 0x0000)?;
    mac_ocp_write(regs, 0xC094, 0x0000)?;
    mac_ocp_write(regs, 0xC09E, 0x0000)?;
    regs.write16(regs::INTR_MITIGATE, 0);
    // The events that take the link out of L1, should firmware have left
    // the link's ASPM on.
    mac_ocp_modify(regs, 0xC0AC, 0, 0x1F80)?;
    Ok(())
}

/// Everything around the chip-specific start: the C+ command, the receive
/// size limit and the rings' addresses under the unlocked configuration
/// registers, then both directions on, the receive and transmit
/// configuration, every multicast let through and the station's frames
/// accepted, and `interrupts` unmasked.
///
/// # Errors
///
/// As [`start_8168h`].
pub fn start<R: Registers, C: Clock>(
    regs: &mut R,
    clock: &mut C,
    cplus: u16,
    tx_ring: u64,
    rx_ring: u64,
    interrupts: u16,
) -> Result<(), Error> {
    unlock(regs);
    regs.write16(regs::CPLUS_CMD, cplus);
    let started = start_8168h(regs, clock);
    regs.write16(regs::RX_MAX_SIZE, regs::RX_MAX);
    // High before low, as Linux writes them.
    regs.write32(regs::TX_DESC_HIGH, (tx_ring >> 32) as u32);
    regs.write32(regs::TX_DESC_LOW, tx_ring as u32);
    regs.write32(regs::RX_DESC_HIGH, (rx_ring >> 32) as u32);
    regs.write32(regs::RX_DESC_LOW, rx_ring as u32);
    lock(regs);
    started?;
    let _ = regs.read8(regs::CHIP_CMD);
    regs.write8(regs::CHIP_CMD, regs::CMD_TX_ENABLE | regs::CMD_RX_ENABLE);
    regs.write32(regs::RX_CONFIG, regs::RX_CONFIG_BASE);
    regs.write32(regs::TX_CONFIG, regs::TX_CONFIG_RUNNING);
    regs.write32(regs::MAR0 + 4, u32::MAX);
    regs.write32(regs::MAR0, u32::MAX);
    regs.write32(
        regs::RX_CONFIG,
        regs::RX_CONFIG_BASE
            | regs::RX_ACCEPT_BROADCAST
            | regs::RX_ACCEPT_MULTICAST
            | regs::RX_ACCEPT_MINE,
    );
    regs.write16(regs::INTR_STATUS, u16::MAX);
    regs.write16(regs::INTR_MASK, interrupts);
    Ok(())
}

/// Stop the chip: interrupts masked and acknowledged, the receiver closed
/// to every frame, the receive path gated and the FIFOs drained, then a
/// reset.
///
/// # Errors
///
/// As [`reset`].
pub fn stop<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<(), Error> {
    regs.write16(regs::INTR_MASK, 0);
    regs.write16(regs::INTR_STATUS, u16::MAX);
    let rx = regs.read32(regs::RX_CONFIG);
    regs.write32(regs::RX_CONFIG, rx & !regs::RX_ACCEPT_MASK);
    quiesce(regs, clock);
    reset(regs, clock)
}
