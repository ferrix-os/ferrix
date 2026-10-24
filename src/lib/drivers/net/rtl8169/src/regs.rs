//! The 8168's register map: byte offsets into its memory-mapped register
//! window (BAR 2 on the RTL8111H), the bits this driver uses, and the
//! indirect address spaces reached through them.
//!
//! Names follow Realtek's datasheets where those are public and Linux's
//! `r8169` where they are not; the values are facts about the chip.

/// The station address, bytes 0 to 3, written only while
/// [`CFG9346`] is unlocked.
pub const MAC0: u32 = 0x00;
/// The station address, bytes 4 and 5, in the low half.
pub const MAC4: u32 = 0x04;
/// The multicast hash filter, 64 bits.
pub const MAR0: u32 = 0x08;
/// The normal-priority transmit ring's address, low and high words.
pub const TX_DESC_LOW: u32 = 0x20;
/// See [`TX_DESC_LOW`].
pub const TX_DESC_HIGH: u32 = 0x24;
/// The chip command register (8 bits).
pub const CHIP_CMD: u32 = 0x37;
/// The transmit poll register (8 bits): the transmit doorbell.
pub const TX_POLL: u32 = 0x38;
/// The interrupt mask (16 bits).
pub const INTR_MASK: u32 = 0x3C;
/// The interrupt status (16 bits), cleared by writing ones.
pub const INTR_STATUS: u32 = 0x3E;
/// The transmit configuration, whose bits 30:20 also carry the chip's
/// identity (its "XID").
pub const TX_CONFIG: u32 = 0x40;
/// The receive configuration.
pub const RX_CONFIG: u32 = 0x44;
/// The configuration registers' write lock (8 bits).
pub const CFG9346: u32 = 0x50;
/// Configuration register 2 (8 bits).
pub const CONFIG2: u32 = 0x53;
/// Configuration register 3 (8 bits).
pub const CONFIG3: u32 = 0x54;
/// Configuration register 5 (8 bits).
pub const CONFIG5: u32 = 0x56;
/// The PHY's status as the MAC sees it (8 bits): link, speed, duplex.
pub const PHY_STATUS: u32 = 0x6C;
/// Extended register data.
pub const ERIDR: u32 = 0x70;
/// Extended register access: command, byte mask and address.
pub const ERIAR: u32 = 0x74;
/// The PCIe PHY's access register.
pub const EPHYAR: u32 = 0x80;
/// The MAC's on-chip-processor bus: command, address and data in one word.
pub const MAC_OCP: u32 = 0xB0;
/// The gigabit PHY's on-chip-processor bus, likewise.
pub const PHY_OCP: u32 = 0xB8;
/// The link power controls (8 bits).
pub const DLLPR: u32 = 0xD0;
/// The microcontroller's status and control (8 bits).
pub const MCU: u32 = 0xD3;
/// The longest frame the receiver writes (16 bits).
pub const RX_MAX_SIZE: u32 = 0xDA;
/// The C+ command register (16 bits).
pub const CPLUS_CMD: u32 = 0xE0;
/// Interrupt moderation (16 bits).
pub const INTR_MITIGATE: u32 = 0xE2;
/// The receive ring's address, low and high words.
pub const RX_DESC_LOW: u32 = 0xE4;
/// See [`RX_DESC_LOW`].
pub const RX_DESC_HIGH: u32 = 0xE8;
/// The largest frame the transmitter sends early (8 bits), in 128 bytes.
pub const MAX_TX_PACKET: u32 = 0xEC;
/// Miscellaneous controls (32 bits).
pub const MISC: u32 = 0xF0;
/// More of them (8 bits).
pub const MISC_1: u32 = 0xF2;

/// [`CHIP_CMD`]: reset, self-clearing when done.
pub const CMD_RESET: u8 = 0x10;
/// [`CHIP_CMD`]: the receiver on.
pub const CMD_RX_ENABLE: u8 = 0x08;
/// [`CHIP_CMD`]: the transmitter on.
pub const CMD_TX_ENABLE: u8 = 0x04;

/// [`TX_POLL`]: the normal-priority queue has descriptors to send.
pub const POLL_NORMAL: u8 = 0x40;

/// [`INTR_STATUS`]: a frame arrived.
pub const INT_RX_OK: u16 = 0x0001;
/// [`INTR_STATUS`]: a frame arrived with an error.
pub const INT_RX_ERR: u16 = 0x0002;
/// [`INTR_STATUS`]: a frame went.
pub const INT_TX_OK: u16 = 0x0004;
/// [`INTR_STATUS`]: a frame failed to go.
pub const INT_TX_ERR: u16 = 0x0008;
/// [`INTR_STATUS`]: the receiver found no descriptor of its own.
pub const INT_RX_OVERFLOW: u16 = 0x0010;
/// [`INTR_STATUS`]: the link changed.
pub const INT_LINK_CHANGE: u16 = 0x0020;
/// [`INTR_STATUS`]: the receive FIFO overflowed.
pub const INT_RX_FIFO_OVER: u16 = 0x0040;
/// [`INTR_STATUS`]: a PCI system error.
pub const INT_SYSTEM_ERROR: u16 = 0x8000;
/// The interrupts the driver takes: what Linux's `r8169` takes on this
/// family.
pub const INTERRUPTS: u16 = INT_RX_OK | INT_RX_ERR | INT_TX_OK | INT_TX_ERR | INT_LINK_CHANGE;

/// [`TX_CONFIG`]: the mask of the identity bits, after shifting right by
/// [`XID_SHIFT`].
pub const XID_MASK: u32 = 0xFCF;
/// See [`XID_MASK`].
pub const XID_SHIFT: u32 = 20;
/// [`TX_CONFIG`]: the transmit FIFO is empty (8168E-VL and later).
pub const TX_EMPTY: u32 = 1 << 11;
/// [`TX_CONFIG`]: the FIFO is sized dynamically (8168E-VL and later).
pub const TX_AUTO_FIFO: u32 = 1 << 7;
/// [`TX_CONFIG`]: unlimited DMA bursts, shortest inter-frame gap.
pub const TX_CONFIG_RUNNING: u32 = (7 << 8) | (3 << 24) | TX_AUTO_FIFO;

/// [`RX_CONFIG`]: accept frames to the station address.
pub const RX_ACCEPT_MINE: u32 = 0x02;
/// [`RX_CONFIG`]: accept multicast frames the hash filter passes.
pub const RX_ACCEPT_MULTICAST: u32 = 0x04;
/// [`RX_CONFIG`]: accept broadcast frames.
pub const RX_ACCEPT_BROADCAST: u32 = 0x08;
/// [`RX_CONFIG`]: every accept bit, the error and runt ones included.
pub const RX_ACCEPT_MASK: u32 = 0x3F;
/// [`RX_CONFIG`] without its accept bits, as Linux sets it for the 8168G
/// to 8168EP: an interrupt per 128 bytes off, multiple reads, unlimited
/// DMA bursts, early receive off.
pub const RX_CONFIG_BASE: u32 = (1 << 15) | (1 << 14) | (7 << 8) | (1 << 11);

/// [`CFG9346`]: the configuration registers writable.
pub const CFG_UNLOCK: u8 = 0xC0;
/// [`CFG9346`]: locked again.
pub const CFG_LOCK: u8 = 0x00;
/// [`CONFIG2`]: the chip may assert `CLKREQ#`.
pub const CONFIG2_CLKREQ: u8 = 1 << 7;
/// [`CONFIG3`]: ready to enter L2/L3; Linux clears it against a reset in
/// those states that the chip mishandles.
pub const CONFIG3_READY_L23: u8 = 1 << 1;
/// [`CONFIG5`]: the chip's own ASPM.
pub const CONFIG5_ASPM: u8 = 1 << 0;

/// [`PHY_STATUS`]: 1000 Mb/s full duplex.
pub const STATUS_1000_FULL: u8 = 0x10;
/// [`PHY_STATUS`]: 100 Mb/s.
pub const STATUS_100: u8 = 0x08;
/// [`PHY_STATUS`]: 10 Mb/s.
pub const STATUS_10: u8 = 0x04;
/// [`PHY_STATUS`]: the link is up.
pub const STATUS_LINK: u8 = 0x02;
/// [`PHY_STATUS`]: full duplex.
pub const STATUS_FULL: u8 = 0x01;

/// [`ERIAR`], [`EPHYAR`], [`MAC_OCP`] and [`PHY_OCP`]: the busy, or for a
/// read the done, flag; written set it is a write command.
pub const ACCESS_FLAG: u32 = 1 << 31;
/// [`ERIAR`]: where the byte-enable mask starts.
pub const ERI_MASK_SHIFT: u32 = 12;
/// [`ERIAR`]: all four bytes.
pub const ERI_MASK_ALL: u32 = 0xF;
/// [`ERIAR`]: the low two bytes.
pub const ERI_MASK_LOW2: u32 = 0x3;
/// [`ERIAR`]: the low byte.
pub const ERI_MASK_LOW1: u32 = 0x1;
/// [`EPHYAR`]: where the register number starts.
pub const EPHY_REG_SHIFT: u32 = 16;
/// [`MAC_OCP`] and [`PHY_OCP`]: where the address starts.
pub const OCP_ADDRESS_SHIFT: u32 = 15;

/// [`DLLPR`]: the PHY's power-saving "PFM" mode.
pub const DLLPR_PFM: u8 = 1 << 6;
/// [`DLLPR`]: power saving at 10 Mb/s.
pub const DLLPR_TX_10M_PS: u8 = 1 << 7;
/// [`MISC_1`]: PFM in D3cold.
pub const MISC_1_PFM_D3COLD: u8 = 1 << 6;
/// [`MCU`]: the out-of-band firmware owns the chip.
pub const MCU_NOW_IS_OOB: u8 = 1 << 7;
/// [`MCU`]: the transmit FIFO is empty.
pub const MCU_TX_EMPTY: u8 = 1 << 5;
/// [`MCU`]: the receive FIFO is empty.
pub const MCU_RX_EMPTY: u8 = 1 << 4;
/// [`MCU`]: the shared FIFO's link list is ready.
pub const MCU_LINK_LIST_READY: u8 = 1 << 1;
/// [`MISC`]: the receive path gated off from the MAC.
pub const MISC_RXDV_GATE: u32 = 1 << 19;

/// [`CPLUS_CMD`]: the bits the driver keeps from firmware's value: normal
/// mode and the interrupt timer's unit. Receive checksumming and VLAN
/// stripping are left off.
pub const CPLUS_KEEP: u16 = (1 << 13) | 0x3;

/// The early-transmit size for the 8168E-VL and later, in 128 bytes.
pub const EARLY_SIZE: u8 = 0x27;

/// The receive size limit the driver writes: Linux's, 16 KiB, so the chip
/// filters nothing by length and a frame longer than a buffer arrives in
/// pieces, which the driver drops.
pub const RX_MAX: u16 = 0x4000;

/// The PHY's registers, as clause 22 numbers them on page 0.
pub mod phy {
    /// Basic control.
    pub const BMCR: u8 = 0x00;
    /// Basic status.
    pub const BMSR: u8 = 0x01;
    /// The identifier, high half.
    pub const PHYSID1: u8 = 0x02;
    /// The identifier, low half.
    pub const PHYSID2: u8 = 0x03;
    /// The 10/100 advertisement.
    pub const ADVERTISE: u8 = 0x04;
    /// The 1000BASE-T control.
    pub const CTRL1000: u8 = 0x09;

    /// [`BMCR`]: software reset, self-clearing.
    pub const BMCR_RESET: u16 = 1 << 15;
    /// [`BMCR`]: auto-negotiation on.
    pub const BMCR_ANENABLE: u16 = 1 << 12;
    /// [`BMCR`]: powered down.
    pub const BMCR_POWER_DOWN: u16 = 1 << 11;
    /// [`BMCR`]: restart auto-negotiation.
    pub const BMCR_ANRESTART: u16 = 1 << 9;

    /// [`ADVERTISE`]: 802.3, 10 and 100 Mb/s in both duplexes, and
    /// symmetric pause.
    pub const ADVERTISE_ALL: u16 = 0x0001 | 0x0020 | 0x0040 | 0x0080 | 0x0100 | 0x0400;
    /// [`CTRL1000`]: 1000 Mb/s full duplex.
    pub const ADVERTISE_1000_FULL: u16 = 0x0200;

    /// Where page 0's registers are on the PHY's OCP bus.
    pub const STANDARD_BASE: u16 = 0xA400;
}
