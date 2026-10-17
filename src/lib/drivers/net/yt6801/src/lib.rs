//! The Motorcomm YT6801's own registers: the part of the chip around its
//! `DesignWare` Ethernet `QoS` core, which `ferrix-dwmac` drives.
//!
//! The YT6801 is a PCIe gigabit adapter, PCI `1F0A:6801`, with the core at
//! offset [`GMAC_OFFSET`] of its BAR 0 and a YT8531S PHY inside it at MDIO
//! address 0. Before the core can be brought up the chip wants three things
//! of its own, all in BAR 0's second page:
//!
//! 1. A **chip reset** ([`glue::SYS_RESET`]) and the **PHY's reset
//!    released** ([`glue::EPHY_CTRL`]): until then the PHY gives the core no
//!    clocks, and the core's own software reset never finishes.
//! 2. The **MAC address**, which is not in a register but in the chip's
//!    eFuse, as a patch to the core's address registers ([`efuse_mac`]).
//! 3. **Interrupt delivery**: its out-of-band wake-on-LAN block holds the
//!    DMA's interrupts back until it is disabled ([`glue::OOB_WOL_CTRL`]),
//!    and its management interrupts are masked ([`enable_interrupts`]).
//!
//! The facts are from Linux's `dwmac-motorcomm.c` and Motorcomm's own
//! driver as posted to netdev (v4, April 2025); no code was taken from
//! either.
//!
//! # The MSI-X table shares the page
//!
//! The chip's MSI-X table and pending-bit array are at `0x1200` and
//! `0x1300` of BAR 0, in the same page as everything above. Ferrix's kernel
//! never lets a driver map a page holding an MSI-X table it uses, so for
//! this chip it does not use MSI-X at all: it holds MSI-X disabled, grants
//! the page as an ordinary aperture, and offers the function's one MSI
//! vector instead (`docs/DEVMGR.md`, the consultant's review of
//! 2026-10-15).
//!
//! # One MSI is the glue's aggregator
//!
//! With MSI-X off the chip's one message comes from an aggregator in the
//! glue ([`glue::MGMT_INT_CTRL0`]): it latches the core's per-channel
//! interrupt lines and sends the MSI as its status goes from clear to set.
//! The latch stays set until software clears it, so a driver that only
//! clears the core's channel status hears the first event and almost none
//! after. The recipe is Motorcomm's own driver's for MSI: the core in
//! interrupt mode 2 ([`options`]), the core's own line masked in the
//! aggregator ([`enable_interrupts`]), and the latch cleared after every
//! drain ([`rearm`]). Linux's `dwmac-motorcomm` runs the chip on MSI-X and
//! does none of it.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests;

use ferrix_dwmac::{Bus, Clock, Options, Registers, regs};

/// Motorcomm's PCI vendor id.
pub const VENDOR: u16 = 0x1F0A;

/// The YT6801's PCI device id.
pub const DEVICE: u16 = 0x6801;

/// Where in BAR 0 the `DesignWare` core's registers start.
pub const GMAC_OFFSET: u32 = 0x2000;

/// How many bytes of BAR 0 a driver needs mapped: the glue's page and the
/// core's registers after it.
pub const BAR_BYTES: u64 = 0x4000;

/// The YT8531S's PHY identifier, as the PHY's registers 2 and 3 read.
pub const PHY_ID: u32 = 0x4F51_E91A;

/// The chip's own registers, by byte offset from BAR 0.
pub mod glue {
    /// The integrated PHY's control: bit 0 clear holds the MDIO bus and the
    /// PHY in reset.
    pub const EPHY_CTRL: u32 = 0x1004;
    /// Bit 0 of [`EPHY_CTRL`]: release the MDIO bus and the PHY.
    pub const EPHY_RELEASE: u32 = 1 << 0;
    /// Out-of-band wake-on-LAN control.
    pub const OOB_WOL_CTRL: u32 = 0x1010;
    /// Bit 0 of [`OOB_WOL_CTRL`]: the block disabled, without which the
    /// DMA's interrupts are never delivered.
    pub const OOB_WOL_DISABLE: u32 = 1 << 0;
    /// The glue's interrupt aggregator: what the chip turns into its one
    /// MSI when MSI-X is off. Status in bits 15:0, latched, one bit per
    /// source ([`MGMT_STATUS_RX`], [`MGMT_STATUS_TX`], [`MGMT_STATUS_MISC`]);
    /// a mask in bits 31:16, a set bit holding that source back.
    pub const MGMT_INT_CTRL0: u32 = 0x1100;
    /// [`MGMT_INT_CTRL0`]'s status bits.
    pub const MGMT_STATUS: u32 = 0xFFFF;
    /// A receive channel's per-channel interrupt, one bit for each of four.
    pub const MGMT_STATUS_RX: u32 = 0x000F;
    /// The transmit channel's per-channel interrupt.
    pub const MGMT_STATUS_TX: u32 = 0x0010;
    /// The core's own line (`sbd_intr_o`): the MAC's, the MTL's and the
    /// DMA's abnormal events.
    pub const MGMT_STATUS_MISC: u32 = 0x0020;
    /// Where [`MGMT_INT_CTRL0`]'s mask starts.
    pub const MGMT_MASK_SHIFT: u32 = 16;
    /// [`MGMT_INT_CTRL0`]'s mask with every source held back.
    pub const MGMT_MASK_ALL: u32 = 0xFFFF << MGMT_MASK_SHIFT;
    /// [`MGMT_INT_CTRL0`] while the driver runs, as Motorcomm's driver
    /// leaves it on one MSI: status clear, the channels' interrupts let
    /// through, the core's own line held back.
    pub const MGMT_RUNNING: u32 = MGMT_STATUS_MISC << MGMT_MASK_SHIFT;
    /// Interrupt moderation: receive in bits 11:0, transmit in 27:16.
    pub const INT_MODERATION: u32 = 0x1108;
    /// The MSI-X table, which the driver never touches.
    pub const MSIX_TABLE: u32 = 0x1200;
    /// The MSI-X pending-bit array, which the driver never touches.
    pub const MSIX_PBA: u32 = 0x1300;
    /// eFuse operation, first word: mode, start, address.
    pub const EFUSE_OP_CTRL_0: u32 = 0x1500;
    /// The operation mode field's "read a row" value.
    pub const EFUSE_ROW_READ: u32 = 0x1;
    /// Start the operation.
    pub const EFUSE_START: u32 = 1 << 2;
    /// The byte address field's shift.
    pub const EFUSE_ADDRESS_SHIFT: u32 = 8;
    /// eFuse operation, second word: done, and the byte read.
    pub const EFUSE_OP_CTRL_1: u32 = 0x1504;
    /// The operation finished.
    pub const EFUSE_DONE: u32 = 1 << 1;
    /// The byte read's shift.
    pub const EFUSE_DATA_SHIFT: u32 = 24;
    /// The chip's reset.
    pub const SYS_RESET: u32 = 0x152C;
    /// Bit 31 of [`SYS_RESET`]: cleared then set resets the chip.
    pub const SYS_RESET_BIT: u32 = 1 << 31;
    /// Where in the eFuse the patch region starts.
    pub const PATCH_REGION: u32 = 18;
    /// Bytes in one patch: a little-endian 16-bit register address, then a
    /// little-endian 32-bit value.
    pub const PATCH_BYTES: u32 = 6;
    /// The most patches the region holds.
    pub const PATCHES: u32 = 39;
    /// The patch address of the core's address register, low word.
    pub const PATCH_MACA0LR: u16 = 0x1520;
    /// The patch address of the core's address register, high word.
    pub const PATCH_MACA0HR: u16 = 0x1524;
}

/// How long one eFuse byte may take: Linux allows 20 ms.
const EFUSE_PATIENCE_NANOS: u64 = 20_000_000;

/// How long to wait between two looks at the eFuse's done bit.
const EFUSE_POLL_NANOS: u64 = 100_000;

/// How long the eFuse needs after a chip reset before it reads anything
/// but zeroes: Linux sleeps 2 to 5 ms.
const AFTER_RESET_NANOS: u64 = 5_000_000;

/// The core's registers, at [`GMAC_OFFSET`] of a mapping of BAR 0.
#[derive(Debug)]
pub struct Gmac<R>(pub R);

impl<R: Registers> Registers for Gmac<R> {
    fn read(&self, offset: u32) -> u32 {
        self.0.read(GMAC_OFFSET + offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        self.0.write(GMAC_OFFSET + offset, value);
    }
}

/// Where the address came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacSource {
    /// The chip's eFuse.
    Efuse,
    /// The core's address registers as firmware left them before the reset.
    Firmware,
    /// Made up, locally administered, from the caller's seed.
    Generated,
}

/// What the eFuse said.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Efuse {
    /// A unicast address.
    Found([u8; 6]),
    /// No address patch, or one that is not a unicast address.
    Absent,
    /// A byte read never finished.
    Timeout,
}

/// The chip after [`prepare`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Prepared {
    /// The address to program.
    pub mac: [u8; 6],
    /// Where it came from.
    pub source: MacSource,
    /// What the eFuse said.
    pub efuse: Efuse,
    /// What the core's address registers held before the reset.
    pub firmware: [u8; 6],
}

/// Whether `mac` is an address an interface may have: not all zeroes and
/// not a group address (which all ones is too).
#[must_use]
pub const fn valid_unicast(mac: [u8; 6]) -> bool {
    let zero =
        mac[0] == 0 && mac[1] == 0 && mac[2] == 0 && mac[3] == 0 && mac[4] == 0 && mac[5] == 0;
    !zero && mac[0] & 1 == 0
}

/// A locally administered unicast address from `seed`: `02:1f:0a` and the
/// seed, so two adapters on one machine differ when their seeds do.
#[must_use]
pub const fn generated(seed: [u8; 3]) -> [u8; 6] {
    [0x02, 0x1F, 0x0A, seed[0], seed[1], seed[2]]
}

/// The address in the core's address registers, read through BAR 0.
fn firmware_mac<R: Registers>(bar: &R) -> [u8; 6] {
    let high = bar.read(GMAC_OFFSET + regs::MAC_ADDRESS0_HIGH);
    let low = bar.read(GMAC_OFFSET + regs::MAC_ADDRESS0_LOW).to_le_bytes();
    [
        low[0],
        low[1],
        low[2],
        low[3],
        high as u8,
        (high >> 8) as u8,
    ]
}

/// Reset the chip and release the PHY's reset, then give the eFuse the
/// time it needs to load.
pub fn reset<R: Registers, C: Clock>(bar: &mut R, clock: &mut C) {
    let value = bar.read(glue::SYS_RESET);
    bar.write(glue::SYS_RESET, value & !glue::SYS_RESET_BIT);
    bar.write(glue::SYS_RESET, value | glue::SYS_RESET_BIT);
    release_phy(bar);
    clock.pause(AFTER_RESET_NANOS);
}

/// Release the MDIO bus and the PHY from their reset, which a chip reset
/// and a return from D3hot both assert.
pub fn release_phy<R: Registers>(bar: &mut R) {
    let value = bar.read(glue::EPHY_CTRL);
    bar.write(glue::EPHY_CTRL, value | glue::EPHY_RELEASE);
}

/// One byte of the eFuse, or `None` if the read never finished.
fn efuse_byte<R: Registers, C: Clock>(bar: &mut R, clock: &mut C, address: u32) -> Option<u8> {
    bar.write(
        glue::EFUSE_OP_CTRL_0,
        glue::EFUSE_ROW_READ | glue::EFUSE_START | ((address & 0xFF) << glue::EFUSE_ADDRESS_SHIFT),
    );
    let deadline = clock.now_nanos().saturating_add(EFUSE_PATIENCE_NANOS);
    loop {
        let value = bar.read(glue::EFUSE_OP_CTRL_1);
        if value & glue::EFUSE_DONE != 0 {
            return Some((value >> glue::EFUSE_DATA_SHIFT) as u8);
        }
        if clock.now_nanos() >= deadline {
            return None;
        }
        clock.pause(EFUSE_POLL_NANOS);
    }
}

/// Patch `index` of the eFuse's patch region: its register address and
/// value.
fn patch<R: Registers, C: Clock>(bar: &mut R, clock: &mut C, index: u32) -> Option<(u16, u32)> {
    let mut bytes = [0_u8; glue::PATCH_BYTES as usize];
    let base = glue::PATCH_REGION + glue::PATCH_BYTES * index;
    for (at, byte) in (base..).zip(bytes.iter_mut()) {
        *byte = efuse_byte(bar, clock, at)?;
    }
    let [a0, a1, d0, d1, d2, d3] = bytes;
    Some((
        u16::from_le_bytes([a0, a1]),
        u32::from_le_bytes([d0, d1, d2, d3]),
    ))
}

/// The MAC address the eFuse patches the core's address registers with.
///
/// The patch region is a list ended by an address of zero; the two words
/// of the address are looked for in one walk of it.
pub fn efuse_mac<R: Registers, C: Clock>(bar: &mut R, clock: &mut C) -> Efuse {
    let mut low = None;
    let mut high = None;
    for index in 0..glue::PATCHES {
        let Some((address, value)) = patch(bar, clock, index) else {
            return Efuse::Timeout;
        };
        match address {
            0 => break,
            glue::PATCH_MACA0LR => low = low.or(Some(value)),
            glue::PATCH_MACA0HR => high = high.or(Some(value)),
            _ => {}
        }
        if low.is_some() && high.is_some() {
            break;
        }
    }
    let (Some(low), Some(high)) = (low, high) else {
        return Efuse::Absent;
    };
    // Unlike the core's own registers, the patch words hold the address
    // most significant byte first.
    let mac = [
        (high >> 8) as u8,
        high as u8,
        (low >> 24) as u8,
        (low >> 16) as u8,
        (low >> 8) as u8,
        low as u8,
    ];
    if valid_unicast(mac) {
        Efuse::Found(mac)
    } else {
        Efuse::Absent
    }
}

/// Everything before the core's bring-up: note what firmware left in the
/// core's address registers, reset the chip, release the PHY, and choose
/// the address -- the eFuse's, else firmware's, else one made from `seed`.
pub fn prepare<R: Registers, C: Clock>(bar: &mut R, clock: &mut C, seed: [u8; 3]) -> Prepared {
    let firmware = firmware_mac(bar);
    reset(bar, clock);
    let efuse = efuse_mac(bar, clock);
    let (mac, source) = match efuse {
        Efuse::Found(mac) => (mac, MacSource::Efuse),
        _ if valid_unicast(firmware) => (firmware, MacSource::Firmware),
        _ => (generated(seed), MacSource::Generated),
    };
    Prepared {
        mac,
        source,
        efuse,
        firmware,
    }
}

/// Let the DMA's interrupts out: the aggregator cleared and its channel
/// sources let through ([`glue::MGMT_RUNNING`]), moderation at 200 each
/// way, and the wake-on-LAN block disabled. After the core's bring-up, and
/// again after a return from D3hot.
///
/// Linux's `dwmac-motorcomm` writes 0 to the aggregator, letting the core's
/// own line through too; it runs the chip on MSI-X, where the aggregator
/// does not decide what is sent. On one MSI it does, and Motorcomm's own
/// driver holds that line back, so this does as well: nothing the driver
/// uses is announced on it, and a source the driver never clears (a MAC
/// or counter status) would hold the aggregator raised for good.
pub fn enable_interrupts<R: Registers>(bar: &mut R) {
    bar.write(glue::MGMT_INT_CTRL0, glue::MGMT_RUNNING);
    bar.write(glue::INT_MODERATION, 200 | (200 << 16));
    bar.write(glue::OOB_WOL_CTRL, glue::OOB_WOL_DISABLE);
}

/// Let the next MSI out: clear what the aggregator latched, and return it
/// (0 if nothing was, in which case nothing is written).
///
/// The aggregator latches each source's interrupt and sends one message as
/// its unmasked status goes from clear to set. Motorcomm's driver clears the
/// latch after every service on MSI, and never on MSI-X: a latch left set
/// sends nothing more. On the N150 box a driver that never cleared it had
/// one interrupt at bring-up and about 209 more in five minutes of
/// downloading tens of thousands of frames (2026-10-17).
///
/// After the drain, as Motorcomm's driver does it: the core's channel
/// status cleared and the rings read first, then the latch. In interrupt
/// mode 2 the per-channel lines are levels, raised while the channel's
/// status is, so an event after the channel's status was cleared still
/// holds its line up here and the aggregator latches it again. The latch
/// is cleared with every source masked and only then unmasked, so a source
/// latched again sends a message as the mask comes off, rather than
/// staying set with no edge to send on -- the frame a drain did not see
/// is announced, never left to the next poll.
///
/// Two writes, because what clears a status bit is not documented:
/// Motorcomm's driver writes the value it read back when it masks, and
/// writes the field 0 when it unmasks, so either a write of 1 or a write of
/// 0 clears it. The first write masks every source and puts back what was
/// read; the second writes the status 0 under [`glue::MGMT_RUNNING`]'s
/// mask. Whichever the chip obeys, the latch holds only what is raised now.
pub fn rearm<R: Registers>(bar: &mut R) -> u32 {
    let status = bar.read(glue::MGMT_INT_CTRL0) & glue::MGMT_STATUS;
    if status != 0 {
        bar.write(glue::MGMT_INT_CTRL0, glue::MGMT_MASK_ALL | status);
        bar.write(glue::MGMT_INT_CTRL0, glue::MGMT_RUNNING);
    }
    status
}

/// The core's options on this chip, for `mac`.
///
/// The MDIO clock range is 2 (a CSR clock of 20 to 35 MHz: the chip runs
/// from a 25 MHz crystal), with preamble suppression, as Motorcomm's
/// driver sets them. The FIFO is believed up to 80 KiB and the receive
/// FIFO split four ways, as the chip's four receive queues share it. The
/// core's interrupt mode is 2 ([`ferrix_dwmac::Options::interrupt_mode`]),
/// as Motorcomm's driver sets it on MSI and MSI-X alike: the per-channel
/// lines are levels, raised while the channel's status is, and those are
/// what the glue's aggregator latches and turns into the one MSI
/// ([`rearm`]). Receive has the most descriptors
/// a ring may have, 128: 5 ms of frames at 300 Mbit/s, for a driver that
/// polls when its interrupt does not come.
#[must_use]
pub fn options(mac: [u8; 6]) -> Options {
    Options {
        mac,
        transmit_entries: 64,
        receive_entries: 128,
        buffer_bytes: 2048,
        bus: Bus {
            clock_range: 2,
            preamble_suppression: true,
            phy: 0,
        },
        gigabit: true,
        interrupt_mode: 2,
        fifo_cap: 81920,
        rx_fifo_share: 4,
    }
}
