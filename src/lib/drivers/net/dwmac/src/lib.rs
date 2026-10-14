//! A Synopsys `DesignWare` Ethernet `QoS` core -- "GMAC4" or "dwmac4" in
//! Linux, release 4.00 and later -- as logic over its registers and the DMA
//! memory it is lent.
//!
//! The core is inside many chips: the Motorcomm YT6801's PCIe gigabit
//! adapter (`ferrix-yt6801` is the part of that chip that is not this
//! core), and the STM32MP15's ETH1 among them. A driver process holds an
//! `IoMapping` of the core's registers and pinned memory whose device
//! addresses an IOMMU domain chose, none of which a unit test can have; so
//! this crate is written against [`Registers`], [`Clock`], [`Memory`] and
//! [`Pages`], and its tests run it against a model of the core
//! (`src/tests/model.rs`) that walks the descriptor rings by device address
//! as the DMA does.
//!
//! # What it drives
//!
//! One DMA channel, one transmit queue and one receive queue, sixteen-byte
//! normal descriptors, one buffer per frame each way, no offloads: the
//! checksums are the network stack's, which computes them anyway. The PHY
//! is reached over the core's MDIO interface and driven through the
//! clause-22 registers alone ([`mdio`]).
//!
//! * **Receive** buffers are the driver's: [`Options::receive_entries`] of
//!   them, [`Options::buffer_bytes`] each, carved in order out of the
//!   receive region and every one lent to the DMA at bring-up. A frame that
//!   arrives is [`Event::Received`] -- an offset and a length in that region
//!   -- and its buffer stays out of the ring until [`Driver::release`].
//! * **Transmit** buffers are the caller's: it writes a frame into the
//!   transmit region, names it with an id, and gets the id back in
//!   [`Event::Sent`] once the DMA has read it.
//!
//! # The tail pointers are doorbells
//!
//! The core walks a ring while the descriptor in front of it is its own,
//! and suspends on one that is not; a write of the tail pointer resumes it.
//! This driver always writes the address one past the ring's last
//! descriptor, which no descriptor has, so the channel never stops at the
//! tail and only the ownership bits decide what it may use -- the value
//! Linux's `stmmac` starts its receive ring with, and the one that holds
//! whatever the core does at the tail's own descriptor.
//!
//! # Trust
//!
//! The core can write the rings and the receive buffers, nothing else. A
//! written-back length is checked against the buffer before it decides a
//! copy ([`desc::received`]); the driver's own records, not the ring, say
//! which descriptors are out; and a fatal bus error stops the driver, which
//! then waits for [`Driver::shutdown`].
//!
//! # Memory is given back only after a reset
//!
//! Dropping a [`Driver`] gives nothing back. [`Driver::shutdown`] stops
//! both directions and resets the core, and only a reset seen to finish
//! returns the memory as [`Teardown::Released`]; a core that will not
//! reset keeps it ([`Teardown::Wedged`]), because it may still write there.

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod desc;
pub mod mdio;
pub mod regs;

#[cfg(test)]
mod tests;

use core::fmt;

use desc::{DESCRIPTOR_BYTES, Received};
pub use mdio::{Bus, Link, Speed};

/// A page, as the device addresses in [`Pages`] count them.
pub const PAGE: usize = 4096;

/// The most descriptors one ring may have: two rings share one page.
pub const MAX_ENTRIES: usize = PAGE / 2 / DESCRIPTOR_BYTES;

/// Where in the ring page the receive ring starts.
const RX_RING_OFFSET: usize = PAGE / 2;

/// How long the core's software reset may take before the core is taken to
/// be wedged: Linux's `stmmac` waits a second, and so does this.
const RESET_PATIENCE_NANOS: u64 = 1_000_000_000;

/// How long to wait between two looks at the reset bit.
const RESET_POLL_NANOS: u64 = 100_000;

/// The core's registers, as 32-bit reads and writes at byte offsets from
/// its base.
pub trait Registers {
    /// Read the register at `offset`.
    fn read(&self, offset: u32) -> u32;
    /// Write the register at `offset`.
    fn write(&mut self, offset: u32, value: u32);
}

/// Time, for the waits a reset and the MDIO bus need.
pub trait Clock {
    /// A monotonic clock, in nanoseconds.
    fn now_nanos(&self) -> u64;
    /// Let about `nanos` pass.
    fn pause(&mut self, nanos: u64);
}

/// Memory the core reaches, by device address.
pub trait Pages {
    /// One device address per [`PAGE`], in order.
    fn device_pages(&self) -> &[u64];
}

/// Memory the core reaches that the driver itself reads and writes: the
/// descriptor rings.
pub trait Memory: Pages {
    /// The little-endian word at `offset`, in one access.
    fn read_u32(&self, offset: usize) -> u32;
    /// Write the little-endian word at `offset`, in one access.
    fn write_u32(&mut self, offset: usize, value: u32);
    /// Order every access before this one before every access after it,
    /// for the core as for this processor.
    fn barrier(&self);
}

/// Everything the driver is lent.
#[derive(Debug)]
pub struct Parts<R, C, M, B> {
    /// The core's registers.
    pub regs: R,
    /// The clock.
    pub clock: C,
    /// One page holding both descriptor rings.
    pub rings: M,
    /// The receive buffers.
    pub receive: B,
    /// The frames to send, which the caller writes.
    pub transmit: B,
}

/// How the driver sets the core up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// The interface's hardware address.
    pub mac: [u8; 6],
    /// Transmit descriptors, at most [`MAX_ENTRIES`].
    pub transmit_entries: u16,
    /// Receive descriptors and buffers, at most [`MAX_ENTRIES`].
    pub receive_entries: u16,
    /// Bytes in each receive buffer: a divisor of [`PAGE`] that holds a
    /// whole frame at the default MTU, so 2048 or 4096.
    pub buffer_bytes: u32,
    /// The MDIO bus and the PHY on it.
    pub bus: Bus,
    /// Whether 1000 Mb/s is advertised.
    pub gigabit: bool,
    /// The DMA mode's interrupt mode field: 0 sends every channel's
    /// interrupt out through the core's one line, 1 and 2 each channel's
    /// on its own, as pulses or levels.
    pub interrupt_mode: u8,
    /// The FIFO the core reports is believed up to this many bytes.
    pub fifo_cap: u32,
    /// The receive FIFO is shared by this many queues, of which this
    /// driver uses the first.
    pub rx_fifo_share: u32,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            mac: [0x02, 0, 0, 0, 0, 1],
            transmit_entries: 128,
            receive_entries: 128,
            buffer_bytes: 2048,
            bus: Bus {
                clock_range: 2,
                preamble_suppression: false,
                phy: 0,
            },
            gigabit: true,
            interrupt_mode: 0,
            fifo_cap: u32::MAX,
            rx_fifo_share: 1,
        }
    }
}

/// What the core and its PHY said of themselves at bring-up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// The version register: the release in its low byte.
    pub version: u32,
    /// Hardware feature register 1.
    pub feature1: u32,
    /// How many address bits the DMA drives.
    pub address_bits: u32,
    /// The PHY's identifier.
    pub phy_id: u32,
    /// The hardware address programmed.
    pub mac: [u8; 6],
}

/// What can go wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The options or the memory do not fit together.
    BadOptions,
    /// A device address is past what the DMA can drive.
    AddressTooWide,
    /// The core's software reset never finished: usually the PHY's clocks
    /// are not running.
    ResetTimeout,
    /// An MDIO operation never finished.
    MdioTimeout,
    /// The PHY never came out of its reset.
    PhyResetTimeout,
    /// No PHY answered; the identifier read is carried.
    NoPhy(u32),
    /// The core reported a fatal bus error and stopped.
    BusError,
    /// The driver failed earlier and does nothing more.
    Broken,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadOptions => f.write_str("the options and the memory do not fit"),
            Error::AddressTooWide => f.write_str("memory past what the DMA can address"),
            Error::ResetTimeout => {
                f.write_str("the core's reset never finished (are the PHY's clocks running?)")
            }
            Error::MdioTimeout => f.write_str("an MDIO operation never finished"),
            Error::PhyResetTimeout => f.write_str("the PHY never came out of its reset"),
            Error::NoPhy(id) => write!(f, "no PHY answered (identifier {id:#010x})"),
            Error::BusError => f.write_str("the core reported a fatal bus error"),
            Error::Broken => f.write_str("the driver failed earlier"),
        }
    }
}

/// A frame to send.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Frame {
    /// The caller's name for it, handed back in [`Event::Sent`].
    pub id: u64,
    /// Where it starts in the transmit region.
    pub offset: u64,
    /// How many bytes: the whole Ethernet frame without its check sequence.
    pub len: u32,
}

/// Why a frame was not taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// Every transmit descriptor is out; take completions and try again.
    QueueFull,
    /// Empty, or longer than a descriptor carries.
    BadLength,
    /// Not inside the transmit region, or across a page boundary.
    OutsideData,
    /// The driver has failed.
    Broken,
}

/// Something the core did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// A frame arrived: `len` bytes from `offset` of the receive region,
    /// there until [`Driver::release`] gives `buffer` back.
    Received {
        /// The buffer it came in.
        buffer: u16,
        /// Where it starts in the receive region.
        offset: u64,
        /// How many bytes, without the check sequence.
        len: u32,
    },
    /// A frame was sent: its bytes are the caller's again.
    Sent {
        /// The id it was submitted with.
        id: u64,
    },
}

/// What one drain found.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Drained {
    /// Events written to the front of the caller's slice.
    pub events: usize,
    /// The channel status bits that were set, and are now clear.
    pub status: u32,
}

/// What has happened since bring-up.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    /// Frames sent.
    pub sent: u64,
    /// Frames the core said it failed to send.
    pub send_errors: u64,
    /// Frames received and handed on.
    pub received: u64,
    /// Receive descriptors given back with an error, a part of a frame or a
    /// length no buffer holds, and lent again unread.
    pub receive_dropped: u64,
    /// Times the core found no receive descriptor of its own.
    pub receive_unavailable: u64,
}

/// A bring-up that failed, with the memory and whether the core was seen
/// reset after the failure -- only then may the memory be freed.
#[derive(Debug)]
pub struct Failed<R, C, M, B> {
    /// Why.
    pub error: Error,
    /// What was lent.
    pub parts: Parts<R, C, M, B>,
    /// Whether the core was reset after the failure, so it reaches no
    /// memory any more.
    pub reset: bool,
}

/// How [`Driver::shutdown`] ended.
#[derive(Debug)]
pub enum Teardown<R, C, M, B> {
    /// The core was reset: the memory may be freed.
    Released(Parts<R, C, M, B>),
    /// The core did not reset: the memory must stay where the core can
    /// still write it.
    Wedged(Parts<R, C, M, B>),
}

/// The driver.
pub struct Driver<R, C, M, B> {
    parts: Parts<R, C, M, B>,
    options: Options,
    info: Info,
    /// Device address of the transmit ring's first descriptor.
    tx_ring: u64,
    /// Device address of the receive ring's first descriptor.
    rx_ring: u64,
    /// The id of the frame each transmit descriptor carries, if it is out.
    tx_ids: [Option<u64>; MAX_ENTRIES],
    /// The next transmit descriptor to fill.
    tx_head: usize,
    /// The oldest transmit descriptor out.
    tx_clean: usize,
    /// How many are out.
    tx_out: usize,
    /// Receive descriptors whose buffers the caller holds.
    rx_held: [bool; MAX_ENTRIES],
    /// The next receive descriptor the core will give back.
    rx_next: usize,
    link: Link,
    counters: Counters,
    broken: bool,
}

impl<R, C, M, B> fmt::Debug for Driver<R, C, M, B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Driver")
            .field("info", &self.info)
            .field("link", &self.link)
            .field("tx_out", &self.tx_out)
            .field("counters", &self.counters)
            .field("broken", &self.broken)
            .finish_non_exhaustive()
    }
}

/// Reset the core and wait for the reset to finish.
fn reset<R: Registers, C: Clock>(regs: &mut R, clock: &mut C) -> Result<(), Error> {
    regs.write(regs::DMA_MODE, regs::DMA_SWR);
    let deadline = clock.now_nanos().saturating_add(RESET_PATIENCE_NANOS);
    loop {
        if regs.read(regs::DMA_MODE) & regs::DMA_SWR == 0 {
            return Ok(());
        }
        if clock.now_nanos() >= deadline {
            return Err(Error::ResetTimeout);
        }
        clock.pause(RESET_POLL_NANOS);
    }
}

/// The device address of byte `offset` of `pages`, if the `len` bytes from
/// it lie inside one page of them.
fn device_address(pages: &[u64], offset: u64, len: u64) -> Option<u64> {
    let page = usize::try_from(offset / PAGE as u64).ok()?;
    let within = offset % PAGE as u64;
    if within.checked_add(len)? > PAGE as u64 {
        return None;
    }
    pages.get(page).map(|base| base + within)
}

/// The FIFO bytes a queue gets, as the operation mode's size field wants
/// them: in 256-byte blocks, less one.
fn fifo_blocks(bytes: u32) -> u32 {
    (bytes / 256).saturating_sub(1)
}

impl<R: Registers, C: Clock, M: Memory, B: Pages> Driver<R, C, M, B> {
    /// Reset the core, start the PHY's auto-negotiation, lay out the rings
    /// and start both DMA directions. The link comes up later: see
    /// [`Driver::poll_link`].
    ///
    /// # Errors
    ///
    /// [`Failed`], with the memory back and whether the core was reset.
    pub fn init(
        mut parts: Parts<R, C, M, B>,
        options: Options,
    ) -> Result<Self, Failed<R, C, M, B>> {
        let checked = Self::check(&parts, &options);
        let (tx_ring, rx_ring) = match checked {
            Ok(rings) => rings,
            Err(error) => {
                return Err(Failed {
                    error,
                    parts,
                    reset: false,
                });
            }
        };
        if let Err(error) = reset(&mut parts.regs, &mut parts.clock) {
            return Err(Failed {
                error,
                parts,
                reset: false,
            });
        }
        let version = parts.regs.read(regs::MAC_VERSION);
        let feature1 = parts.regs.read(regs::MAC_HW_FEATURE1);
        let address_bits = regs::address_bits(feature1);
        let mut driver = Driver {
            parts,
            options,
            info: Info {
                version,
                feature1,
                address_bits,
                phy_id: 0,
                mac: options.mac,
            },
            tx_ring,
            rx_ring,
            tx_ids: [None; MAX_ENTRIES],
            tx_head: 0,
            tx_clean: 0,
            tx_out: 0,
            rx_held: [false; MAX_ENTRIES],
            rx_next: 0,
            link: Link::Down,
            counters: Counters::default(),
            broken: false,
        };
        match driver.bring_up() {
            Ok(()) => Ok(driver),
            Err(error) => {
                let Driver { mut parts, .. } = driver;
                let reset = reset(&mut parts.regs, &mut parts.clock).is_ok();
                Err(Failed {
                    error,
                    parts,
                    reset,
                })
            }
        }
    }

    /// The rings' device addresses, if the options and the memory fit.
    fn check(parts: &Parts<R, C, M, B>, options: &Options) -> Result<(u64, u64), Error> {
        let tx = usize::from(options.transmit_entries);
        let rx = usize::from(options.receive_entries);
        let buffer = options.buffer_bytes as usize;
        let fits = (1..=MAX_ENTRIES).contains(&tx)
            && (1..=MAX_ENTRIES).contains(&rx)
            && (1536..=PAGE).contains(&buffer)
            && PAGE.is_multiple_of(buffer)
            && parts.receive.device_pages().len() * PAGE >= rx * buffer;
        let ring = parts.rings.device_pages().first().copied();
        match ring {
            Some(ring) if fits && ring % PAGE as u64 == 0 => {
                Ok((ring, ring + RX_RING_OFFSET as u64))
            }
            _ => Err(Error::BadOptions),
        }
    }

    /// Everything after the reset, in the order the core wants it.
    fn bring_up(&mut self) -> Result<(), Error> {
        self.check_addresses()?;
        let bus = self.options.bus;
        let regs = &mut self.parts.regs;
        let clock = &mut self.parts.clock;
        self.info.phy_id = mdio::identify(regs, clock, bus)?;
        mdio::start(regs, clock, bus, self.options.gigabit)?;
        self.configure_dma();
        self.configure_mtl();
        self.configure_mac();
        self.arm_receive_ring();
        self.start();
        Ok(())
    }

    /// Refuse memory past what the DMA can drive.
    fn check_addresses(&self) -> Result<(), Error> {
        let limit = 1_u64 << self.info.address_bits;
        let all = self
            .parts
            .rings
            .device_pages()
            .iter()
            .chain(self.parts.receive.device_pages())
            .chain(self.parts.transmit.device_pages());
        for page in all {
            if page.checked_add(PAGE as u64).is_none_or(|end| end > limit) {
                return Err(Error::AddressTooWide);
            }
        }
        Ok(())
    }

    /// The system bus, the interrupt mode and channel 0.
    fn configure_dma(&mut self) {
        let mut sysbus = regs::SYSBUS_BLEN_4_TO_32
            | (1 << regs::SYSBUS_RD_OSR_SHIFT)
            | (1 << regs::SYSBUS_WR_OSR_SHIFT);
        if self.info.address_bits > 32 {
            sysbus |= regs::SYSBUS_EAME;
        }
        let tx_entries = u32::from(self.options.transmit_entries);
        let rx_entries = u32::from(self.options.receive_entries);
        let regs = &mut self.parts.regs;
        regs.write(regs::DMA_SYSBUS_MODE, sysbus);
        regs.write(
            regs::DMA_MODE,
            u32::from(self.options.interrupt_mode & 0b11) << regs::DMA_INTM_SHIFT,
        );
        regs.write(regs::CH_CONTROL, regs::CH_PBLX8);
        regs.write(
            regs::CH_TX_CONTROL,
            regs::CH_TX_OSP | (32 << regs::CH_PBL_SHIFT),
        );
        regs.write(
            regs::CH_RX_CONTROL,
            (self.options.buffer_bytes << regs::CH_RBSZ_SHIFT) | (32 << regs::CH_PBL_SHIFT),
        );
        regs.write(regs::CH_TX_LIST_HIGH, (self.tx_ring >> 32) as u32);
        regs.write(regs::CH_TX_LIST_LOW, self.tx_ring as u32);
        regs.write(regs::CH_RX_LIST_HIGH, (self.rx_ring >> 32) as u32);
        regs.write(regs::CH_RX_LIST_LOW, self.rx_ring as u32);
        regs.write(regs::CH_TX_RING_LENGTH, tx_entries - 1);
        regs.write(regs::CH_RX_RING_LENGTH, rx_entries - 1);
        let status = regs.read(regs::CH_STATUS);
        regs.write(regs::CH_STATUS, status);
        regs.write(regs::CH_INTERRUPT_ENABLE, regs::CH_INTERRUPTS);
    }

    /// One transmit and one receive queue, both store-and-forward, each
    /// given its share of the FIFO.
    fn configure_mtl(&mut self) {
        let feature1 = self.info.feature1;
        let cap = self.options.fifo_cap;
        let tx = regs::tx_fifo_bytes(feature1).min(cap);
        let rx = regs::rx_fifo_bytes(feature1).min(cap) / self.options.rx_fifo_share.max(1);
        let regs = &mut self.parts.regs;
        regs.write(
            regs::MTL_TXQ0_OPERATION_MODE,
            regs::TXQ_TSF | regs::TXQ_ENABLED | (fifo_blocks(tx) << regs::TXQ_TQS_SHIFT),
        );
        regs.write(
            regs::MTL_RXQ0_OPERATION_MODE,
            regs::RXQ_RSF | (fifo_blocks(rx) << regs::RXQ_RQS_SHIFT),
        );
        regs.write(regs::MTL_Q0_INTERRUPT, 0);
        regs.write(regs::MAC_RXQ_CTRL0, regs::RXQ0_ENABLED);
    }

    /// The address, the filter, the frame check sequence stripped, and no
    /// interrupts of the MAC's own or its counters'. The transmitter and
    /// receiver stay off until the link is up.
    fn configure_mac(&mut self) {
        let [a, b, c, d, e, f] = self.options.mac;
        let regs = &mut self.parts.regs;
        regs.write(
            regs::MAC_ADDRESS0_HIGH,
            regs::ADDRESS_ENABLE | u32::from(e) | (u32::from(f) << 8),
        );
        regs.write(regs::MAC_ADDRESS0_LOW, u32::from_le_bytes([a, b, c, d]));
        regs.write(regs::MAC_PACKET_FILTER, regs::FILTER_PM);
        regs.write(
            regs::MAC_CONFIGURATION,
            regs::MAC_CST | regs::MAC_ACS | regs::MAC_DM,
        );
        regs.write(regs::MAC_INTERRUPT_ENABLE, 0);
        regs.write(regs::MMC_RX_INTERRUPT_MASK, u32::MAX);
        regs.write(regs::MMC_TX_INTERRUPT_MASK, u32::MAX);
        regs.write(regs::MMC_IPC_RX_INTERRUPT_MASK, u32::MAX);
    }

    /// Lend the core every receive buffer, and empty the transmit ring.
    fn arm_receive_ring(&mut self) {
        for index in 0..usize::from(self.options.transmit_entries) {
            self.write_descriptor(index * DESCRIPTOR_BYTES, [0; 4]);
        }
        for index in 0..usize::from(self.options.receive_entries) {
            self.arm(index);
        }
    }

    /// Start both directions.
    fn start(&mut self) {
        let regs = &mut self.parts.regs;
        let tx = regs.read(regs::CH_TX_CONTROL);
        regs.write(regs::CH_TX_CONTROL, tx | regs::CH_TX_ST);
        let rx = regs.read(regs::CH_RX_CONTROL);
        regs.write(regs::CH_RX_CONTROL, rx | regs::CH_RX_SR);
        self.ring_receive();
    }

    /// Write a descriptor's four words, the fourth -- which hands it over
    /// -- after the other three are visible.
    fn write_descriptor(&mut self, at: usize, words: [u32; 4]) {
        let [zero, one, two, three] = words;
        let rings = &mut self.parts.rings;
        rings.write_u32(at, zero);
        rings.write_u32(at + 4, one);
        rings.write_u32(at + 8, two);
        rings.barrier();
        rings.write_u32(at + 12, three);
        rings.barrier();
    }

    /// The device address of receive buffer `index`.
    fn buffer_address(&self, index: usize) -> Option<u64> {
        let buffer = u64::from(self.options.buffer_bytes);
        device_address(
            self.parts.receive.device_pages(),
            index as u64 * buffer,
            buffer,
        )
    }

    /// Lend receive buffer `index` to the core.
    fn arm(&mut self, index: usize) {
        // `check` held every buffer inside the region, so there is an
        // address; a zero one is a descriptor the core will never fill.
        let address = self.buffer_address(index).unwrap_or(0);
        self.write_descriptor(
            RX_RING_OFFSET + index * DESCRIPTOR_BYTES,
            desc::receive(address),
        );
    }

    /// The receive ring's doorbell.
    fn ring_receive(&mut self) {
        let end = self.rx_ring + u64::from(self.options.receive_entries) * DESCRIPTOR_BYTES as u64;
        self.parts.regs.write(regs::CH_RX_TAIL, end as u32);
    }

    /// The transmit ring's doorbell.
    fn ring_transmit(&mut self) {
        let end = self.tx_ring + u64::from(self.options.transmit_entries) * DESCRIPTOR_BYTES as u64;
        self.parts.regs.write(regs::CH_TX_TAIL, end as u32);
    }

    /// What the core and the PHY said at bring-up.
    #[must_use]
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// The link as last polled.
    #[must_use]
    pub fn link(&self) -> Link {
        self.link
    }

    /// What has happened.
    #[must_use]
    pub fn counters(&self) -> Counters {
        self.counters
    }

    /// The core's registers, for what lies beside the core in the same
    /// chip.
    pub fn registers(&mut self) -> &mut R {
        &mut self.parts.regs
    }

    /// Ask the PHY about the link and, if it changed, set the MAC's speed
    /// and duplex to match and switch its transmitter and receiver on or
    /// off. Answers the new link when it changed.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] after a failure, or the MDIO bus's.
    pub fn poll_link(&mut self) -> Result<Option<Link>, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let now = mdio::link(
            &mut self.parts.regs,
            &mut self.parts.clock,
            self.options.bus,
        )?;
        if now == self.link {
            return Ok(None);
        }
        self.link = now;
        let regs = &mut self.parts.regs;
        let mut config = regs.read(regs::MAC_CONFIGURATION)
            & !(regs::MAC_PS | regs::MAC_FES | regs::MAC_DM | regs::MAC_TE | regs::MAC_RE);
        if let Link::Up { speed, full_duplex } = now {
            config |= match speed {
                Speed::Thousand => 0,
                Speed::Hundred => regs::MAC_PS | regs::MAC_FES,
                Speed::Ten => regs::MAC_PS,
            };
            if full_duplex {
                config |= regs::MAC_DM;
            }
            config |= regs::MAC_TE | regs::MAC_RE;
        }
        regs.write(regs::MAC_CONFIGURATION, config);
        Ok(Some(now))
    }

    /// Hand `frame` to the core to send.
    ///
    /// # Errors
    ///
    /// [`SubmitError`]; [`SubmitError::QueueFull`] is the one to wait out.
    pub fn submit(&mut self, frame: &Frame) -> Result<(), SubmitError> {
        if self.broken {
            return Err(SubmitError::Broken);
        }
        if frame.len == 0 || frame.len > desc::TX_LENGTH {
            return Err(SubmitError::BadLength);
        }
        let entries = usize::from(self.options.transmit_entries);
        if self.tx_out >= entries {
            return Err(SubmitError::QueueFull);
        }
        let address = device_address(
            self.parts.transmit.device_pages(),
            frame.offset,
            u64::from(frame.len),
        )
        .ok_or(SubmitError::OutsideData)?;
        let index = self.tx_head;
        let slot = self.tx_ids.get_mut(index).ok_or(SubmitError::Broken)?;
        *slot = Some(frame.id);
        self.write_descriptor(index * DESCRIPTOR_BYTES, desc::transmit(address, frame.len));
        self.tx_head = (index + 1) % entries;
        self.tx_out += 1;
        self.ring_transmit();
        Ok(())
    }

    /// Give receive buffer `buffer` back to the core.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] after a failure, or [`Error::BadOptions`] for a
    /// buffer the caller did not hold.
    pub fn release(&mut self, buffer: u16) -> Result<(), Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let index = usize::from(buffer);
        match self.rx_held.get_mut(index) {
            Some(held) if *held => *held = false,
            _ => return Err(Error::BadOptions),
        }
        self.arm(index);
        self.ring_receive();
        Ok(())
    }

    /// Clear the channel's status, then take what the core finished: sent
    /// frames first, then arrived ones, as many as `out` holds.
    ///
    /// # Errors
    ///
    /// [`Error::BusError`] on a fatal bus error, after which the driver
    /// does nothing more; [`Error::Broken`] after a failure.
    pub fn drain(&mut self, out: &mut [Event]) -> Result<Drained, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        // Cleared before the rings are read, so a frame finished after the
        // read raises the status again rather than being lost between.
        let status = self.parts.regs.read(regs::CH_STATUS);
        self.parts.regs.write(regs::CH_STATUS, status);
        if status & regs::CH_FBE != 0 {
            self.broken = true;
            return Err(Error::BusError);
        }
        if status & regs::CH_RBU != 0 {
            self.counters.receive_unavailable += 1;
        }
        let mut events = self.take_sent(out);
        events += self.take_received(out.get_mut(events..).unwrap_or_default());
        Ok(Drained { events, status })
    }

    /// Take the transmit descriptors the core gave back, oldest first.
    fn take_sent(&mut self, out: &mut [Event]) -> usize {
        let entries = usize::from(self.options.transmit_entries);
        let mut written = 0;
        while self.tx_out > 0 && written < out.len() {
            let index = self.tx_clean;
            let at = index * DESCRIPTOR_BYTES;
            let word3 = self.parts.rings.read_u32(at + 12);
            if word3 & desc::OWN != 0 {
                break;
            }
            self.parts.rings.barrier();
            let id = self.tx_ids.get_mut(index).and_then(Option::take);
            if word3 & desc::TX_ES != 0 {
                self.counters.send_errors += 1;
            } else {
                self.counters.sent += 1;
            }
            if let (Some(id), Some(slot)) = (id, out.get_mut(written)) {
                *slot = Event::Sent { id };
                written += 1;
            }
            self.tx_clean = (index + 1) % entries;
            self.tx_out -= 1;
        }
        written
    }

    /// Take the receive descriptors the core gave back, in ring order,
    /// stopping at one it still owns or one whose buffer the caller holds.
    fn take_received(&mut self, out: &mut [Event]) -> usize {
        let entries = usize::from(self.options.receive_entries);
        let buffer = self.options.buffer_bytes;
        let mut written = 0;
        let mut rearmed = false;
        for _ in 0..entries {
            let index = self.rx_next;
            if self.rx_held.get(index).copied().unwrap_or(true) || written >= out.len() {
                break;
            }
            let at = RX_RING_OFFSET + index * DESCRIPTOR_BYTES;
            let word3 = self.parts.rings.read_u32(at + 12);
            if word3 & desc::OWN != 0 {
                break;
            }
            self.parts.rings.barrier();
            match desc::received(word3, buffer) {
                Received::Frame(len) => {
                    if let (Some(slot), Some(held)) =
                        (out.get_mut(written), self.rx_held.get_mut(index))
                    {
                        *held = true;
                        *slot = Event::Received {
                            buffer: index as u16,
                            offset: index as u64 * u64::from(buffer),
                            len,
                        };
                        written += 1;
                        self.counters.received += 1;
                    }
                }
                Received::Dropped => {
                    self.counters.receive_dropped += 1;
                    self.arm(index);
                    rearmed = true;
                }
            }
            self.rx_next = (index + 1) % entries;
        }
        if rearmed {
            self.ring_receive();
        }
        written
    }

    /// Stop both directions and reset the core.
    pub fn shutdown(self) -> Teardown<R, C, M, B> {
        let Driver { mut parts, .. } = self;
        let regs = &mut parts.regs;
        let tx = regs.read(regs::CH_TX_CONTROL);
        regs.write(regs::CH_TX_CONTROL, tx & !regs::CH_TX_ST);
        let mac = regs.read(regs::MAC_CONFIGURATION);
        regs.write(
            regs::MAC_CONFIGURATION,
            mac & !(regs::MAC_TE | regs::MAC_RE),
        );
        let rx = regs.read(regs::CH_RX_CONTROL);
        regs.write(regs::CH_RX_CONTROL, rx & !regs::CH_RX_SR);
        match reset(&mut parts.regs, &mut parts.clock) {
            Ok(()) => Teardown::Released(parts),
            Err(_) => Teardown::Wedged(parts),
        }
    }
}
