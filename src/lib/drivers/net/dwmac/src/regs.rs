//! The core's registers, by byte offset from its base, and the fields this
//! driver writes or reads.
//!
//! The layout is the `DesignWare` Ethernet `QoS` core's from release 4.00 on
//! (Linux calls it "GMAC4" and drives it with `dwmac4_*`); the Motorcomm
//! YT6801 places it at its BAR's offset `0x2000` and its own driver's
//! register list (`yt6801_type.h`, v4 of the series posted to netdev in
//! April 2025) gives the same offsets from there. Facts only: no code was
//! taken from either.

/// MAC configuration.
pub const MAC_CONFIGURATION: u32 = 0x000;
/// Receiver enable.
pub const MAC_RE: u32 = 1 << 0;
/// Transmitter enable.
pub const MAC_TE: u32 = 1 << 1;
/// Full duplex.
pub const MAC_DM: u32 = 1 << 13;
/// Speed: with [`MAC_PS`], 100 Mb/s set and 10 Mb/s clear.
pub const MAC_FES: u32 = 1 << 14;
/// Port select: set for 10 or 100 Mb/s, clear for 1000.
pub const MAC_PS: u32 = 1 << 15;
/// Strip the pad and frame check sequence of a length-typed frame.
pub const MAC_ACS: u32 = 1 << 20;
/// Strip the frame check sequence of every frame.
pub const MAC_CST: u32 = 1 << 21;

/// MAC packet filter.
pub const MAC_PACKET_FILTER: u32 = 0x008;
/// Pass every multicast frame: IPv6's neighbour discovery needs its
/// solicited-node groups, and this core's hash filter is not programmed.
pub const FILTER_PM: u32 = 1 << 4;

/// Which receive queues are enabled, two bits each.
pub const MAC_RXQ_CTRL0: u32 = 0x0A0;
/// Queue 0 enabled for generic traffic (the field's value 2).
pub const RXQ0_ENABLED: u32 = 0b10;

/// MAC interrupt enable: none are used.
pub const MAC_INTERRUPT_ENABLE: u32 = 0x0B4;

/// MAC PMT control and status: power-down, and the wake-up frames that end
/// it. With `PWRDWN` set the receiver drops every frame but a wake-up one.
pub const MAC_PMT_CONTROL_STATUS: u32 = 0x0C0;

/// The core's release, in its low byte as `0x41` for 4.10 and so on.
pub const MAC_VERSION: u32 = 0x110;
/// Hardware feature register 1: FIFO sizes and address width.
pub const MAC_HW_FEATURE1: u32 = 0x120;

/// MDIO address and command.
pub const MDIO_ADDRESS: u32 = 0x200;
/// MDIO data: the value written, or read back.
pub const MDIO_DATA: u32 = 0x204;
/// Busy: set to start an operation, clear when it finished.
pub const MDIO_GB: u32 = 1 << 0;
/// The operation, bits 3:2: write.
pub const MDIO_GOC_WRITE: u32 = 0b01 << 2;
/// The operation, bits 3:2: read.
pub const MDIO_GOC_READ: u32 = 0b11 << 2;
/// The MDC clock range field's shift (bits 11:8).
pub const MDIO_CR_SHIFT: u32 = 8;
/// The register address field's shift (bits 20:16).
pub const MDIO_RDA_SHIFT: u32 = 16;
/// The PHY address field's shift (bits 25:21).
pub const MDIO_PA_SHIFT: u32 = 21;
/// Preamble suppression.
pub const MDIO_PSE: u32 = 1 << 27;

/// MAC address 0, high: bytes 4 and 5 in bits 15:0.
pub const MAC_ADDRESS0_HIGH: u32 = 0x300;
/// MAC address 0, low: bytes 0 to 3, byte 0 lowest.
pub const MAC_ADDRESS0_LOW: u32 = 0x304;
/// Address enable, which address 0 always has.
pub const ADDRESS_ENABLE: u32 = 1 << 31;

/// The MMC counters' receive interrupt mask: every bit set masks every one.
pub const MMC_RX_INTERRUPT_MASK: u32 = 0x70C;
/// The MMC counters' transmit interrupt mask.
pub const MMC_TX_INTERRUPT_MASK: u32 = 0x710;
/// The MMC checksum-offload counters' receive interrupt mask.
pub const MMC_IPC_RX_INTERRUPT_MASK: u32 = 0x800;

/// Transmit queue 0's operation mode.
pub const MTL_TXQ0_OPERATION_MODE: u32 = 0xD00;
/// Flush the transmit queue; clears itself.
pub const TXQ_FTQ: u32 = 1 << 0;
/// Transmit store and forward: a frame leaves only once wholly in the FIFO.
pub const TXQ_TSF: u32 = 1 << 1;
/// Queue enabled (the field's value 2 in bits 3:2).
pub const TXQ_ENABLED: u32 = 0b10 << 2;
/// The queue size field's shift (bits 24:16), in 256-byte blocks less one.
pub const TXQ_TQS_SHIFT: u32 = 16;
/// Transmit queue 0's interrupt control and status.
pub const MTL_Q0_INTERRUPT: u32 = 0xD2C;
/// Receive queue 0's operation mode.
pub const MTL_RXQ0_OPERATION_MODE: u32 = 0xD30;
/// Receive store and forward.
pub const RXQ_RSF: u32 = 1 << 5;
/// The queue size field's shift (bits 29:20), in 256-byte blocks less one.
pub const RXQ_RQS_SHIFT: u32 = 20;

/// DMA mode.
pub const DMA_MODE: u32 = 0x1000;
/// Software reset of the whole core; clears itself once done. It finishes
/// only with the PHY's clocks running.
pub const DMA_SWR: u32 = 1 << 0;
/// The interrupt mode field's shift (bits 17:16).
pub const DMA_INTM_SHIFT: u32 = 16;
/// System bus mode.
pub const DMA_SYSBUS_MODE: u32 = 0x1004;
/// Bursts of 4, 8, 16 and 32 beats allowed.
pub const SYSBUS_BLEN_4_TO_32: u32 = 0b1111 << 1;
/// Enhanced addressing: descriptors and buffers above 4 GiB.
pub const SYSBUS_EAME: u32 = 1 << 11;
/// The read requests outstanding limit's shift.
pub const SYSBUS_RD_OSR_SHIFT: u32 = 16;
/// The write requests outstanding limit's shift.
pub const SYSBUS_WR_OSR_SHIFT: u32 = 24;

/// DMA channel 0's registers begin here.
pub const DMA_CH0: u32 = 0x1100;
/// Channel control.
pub const CH_CONTROL: u32 = DMA_CH0;
/// The programmable burst lengths count eight times.
pub const CH_PBLX8: u32 = 1 << 16;
/// Channel transmit control.
pub const CH_TX_CONTROL: u32 = DMA_CH0 + 0x04;
/// Start transmitting.
pub const CH_TX_ST: u32 = 1 << 0;
/// Operate on a second frame before the first's status is written back.
pub const CH_TX_OSP: u32 = 1 << 4;
/// The burst length field's shift (bits 21:16), for both directions.
pub const CH_PBL_SHIFT: u32 = 16;
/// Channel receive control.
pub const CH_RX_CONTROL: u32 = DMA_CH0 + 0x08;
/// Start receiving.
pub const CH_RX_SR: u32 = 1 << 0;
/// The receive buffer size field's shift: the size in bytes goes in bits
/// 14:1, so it is written shifted by one.
pub const CH_RBSZ_SHIFT: u32 = 1;
/// The transmit ring's address, upper 32 bits.
pub const CH_TX_LIST_HIGH: u32 = DMA_CH0 + 0x10;
/// The transmit ring's address, lower 32 bits.
pub const CH_TX_LIST_LOW: u32 = DMA_CH0 + 0x14;
/// The receive ring's address, upper 32 bits.
pub const CH_RX_LIST_HIGH: u32 = DMA_CH0 + 0x18;
/// The receive ring's address, lower 32 bits.
pub const CH_RX_LIST_LOW: u32 = DMA_CH0 + 0x1C;
/// The transmit tail pointer, lower 32 bits: a write resumes the channel.
pub const CH_TX_TAIL: u32 = DMA_CH0 + 0x20;
/// The receive tail pointer, lower 32 bits: a write resumes the channel.
pub const CH_RX_TAIL: u32 = DMA_CH0 + 0x28;
/// The transmit ring's length, in descriptors less one.
pub const CH_TX_RING_LENGTH: u32 = DMA_CH0 + 0x2C;
/// The receive ring's length, in descriptors less one.
pub const CH_RX_RING_LENGTH: u32 = DMA_CH0 + 0x30;
/// Channel interrupt enable.
pub const CH_INTERRUPT_ENABLE: u32 = DMA_CH0 + 0x34;
/// Channel status: each bit is cleared by writing it as one.
pub const CH_STATUS: u32 = DMA_CH0 + 0x60;

/// Transmit interrupt: a descriptor with interrupt-on-completion was done.
pub const CH_TI: u32 = 1 << 0;
/// Transmit buffer unavailable: the channel suspended on a descriptor it
/// does not own.
pub const CH_TBU: u32 = 1 << 2;
/// Receive interrupt.
pub const CH_RI: u32 = 1 << 6;
/// Receive buffer unavailable: a frame came with no descriptor to put it in.
pub const CH_RBU: u32 = 1 << 7;
/// Fatal bus error: the channel stopped, and only a reset restarts it.
pub const CH_FBE: u32 = 1 << 12;
/// Abnormal interrupt summary.
pub const CH_AIS: u32 = 1 << 14;
/// Normal interrupt summary.
pub const CH_NIS: u32 = 1 << 15;

/// The channel interrupts the driver asks for: frames done each way, a
/// receive ring run dry, a fatal bus error, and the two summaries the
/// enables of the others go through.
pub const CH_INTERRUPTS: u32 = CH_TI | CH_RI | CH_RBU | CH_FBE | CH_AIS | CH_NIS;

/// Bits 4:0 of hardware feature register 1: the receive FIFO, as `128 <<`
/// this many bytes.
#[must_use]
pub const fn rx_fifo_bytes(feature1: u32) -> u32 {
    128 << (feature1 & 0x1F)
}

/// Bits 10:6 of hardware feature register 1: the transmit FIFO.
#[must_use]
pub const fn tx_fifo_bytes(feature1: u32) -> u32 {
    128 << ((feature1 >> 6) & 0x1F)
}

/// Bits 15:14 of hardware feature register 1: how many bits of address the
/// DMA drives -- 32, 40 or 48.
#[must_use]
pub const fn address_bits(feature1: u32) -> u32 {
    match (feature1 >> 14) & 0b11 {
        1 => 40,
        2 => 48,
        _ => 32,
    }
}

/// The clause-22 PHY registers this driver uses.
pub mod phy {
    /// Basic mode control.
    pub const BMCR: u8 = 0;
    /// Reset; clears itself.
    pub const BMCR_RESET: u16 = 1 << 15;
    /// Auto-negotiation enabled.
    pub const BMCR_ANENABLE: u16 = 1 << 12;
    /// Restart auto-negotiation; clears itself.
    pub const BMCR_ANRESTART: u16 = 1 << 9;
    /// Basic mode status.
    pub const BMSR: u8 = 1;
    /// Link up; latched low, so the second of two reads is now.
    pub const BMSR_LINK: u16 = 1 << 2;
    /// Auto-negotiation finished.
    pub const BMSR_ANEGCOMPLETE: u16 = 1 << 5;
    /// The identifier's high half.
    pub const PHYSID1: u8 = 2;
    /// The identifier's low half.
    pub const PHYSID2: u8 = 3;
    /// What this end advertises.
    pub const ADVERTISE: u8 = 4;
    /// The IEEE 802.3 selector field.
    pub const ADVERTISE_CSMA: u16 = 0x0001;
    /// 10BASE-T half duplex.
    pub const ADVERTISE_10HALF: u16 = 1 << 5;
    /// 10BASE-T full duplex.
    pub const ADVERTISE_10FULL: u16 = 1 << 6;
    /// 100BASE-TX half duplex.
    pub const ADVERTISE_100HALF: u16 = 1 << 7;
    /// 100BASE-TX full duplex.
    pub const ADVERTISE_100FULL: u16 = 1 << 8;
    /// What the link partner advertised, in [`ADVERTISE`]'s bits.
    pub const LPA: u8 = 5;
    /// 1000BASE-T control: what this end advertises of gigabit.
    pub const CTRL1000: u8 = 9;
    /// 1000BASE-T half duplex.
    pub const ADVERTISE_1000HALF: u16 = 1 << 8;
    /// 1000BASE-T full duplex.
    pub const ADVERTISE_1000FULL: u16 = 1 << 9;
    /// 1000BASE-T status: what the partner advertised of gigabit.
    pub const STAT1000: u8 = 10;
    /// The partner can 1000BASE-T half duplex.
    pub const LPA_1000HALF: u16 = 1 << 10;
    /// The partner can 1000BASE-T full duplex.
    pub const LPA_1000FULL: u16 = 1 << 11;
}
