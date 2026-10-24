//! Realtek's 8168 family of PCIe gigabit adapters -- the RTL8111H on
//! nazuna's board among them -- as logic over the chip's registers and the
//! DMA memory it is lent.
//!
//! A driver process holds an `IoMapping` of the chip's register window and
//! pinned memory whose device addresses the IOMMU domain chose, none of
//! which a unit test can have; so this crate is written against
//! [`Registers`], [`Clock`], [`Memory`] and [`Pages`], and its tests run it
//! against a model of the chip (`src/tests/model.rs`) that walks the
//! descriptor rings by device address as the DMA does.
//!
//! # Which chips
//!
//! The RTL8168H / RTL8111H (XID 0x541) and the RTL8168M Realtek handles as
//! one (XID 0x6C0): Linux's `RTL_GIGA_MAC_VER_46`. Any other XID is
//! refused before anything is written ([`Error::Unsupported`]), since each
//! member of the family wants its own set-up and a wrong one can leave a
//! chip that links and moves nothing.
//!
//! # What it drives
//!
//! One transmit ring (the normal-priority queue) and one receive ring,
//! sixteen-byte descriptors ([`desc`]), one buffer per frame each way, no
//! offloads: no checksums, segmentation or VLAN tags in hardware. The net
//! ring carries no "checksum verified" flag to the kernel, so a receive
//! checksum the chip checked would be checked again anyway; and the
//! transmit offloads are where this family's errata live.
//!
//! * **Receive** buffers are the driver's: [`Options::receive_entries`] of
//!   them, [`Options::buffer_bytes`] each, carved in order out of the
//!   receive region and every one lent to the chip at bring-up. A frame
//!   that arrives is [`Event::Received`] and its buffer stays out of the
//!   ring until [`Driver::release`].
//! * **Transmit** buffers are the caller's: it writes a frame into the
//!   transmit region, names it with an id, and gets the id back in
//!   [`Event::Sent`] once the chip has read it.
//!
//! # The chip's quirks, and where they come from
//!
//! The register map, the indirect buses and the RTL8111H's set-up values
//! are facts about the chip read from Linux's `r8169` (`r8169_main.c`,
//! `r8169_phy_config.c`, 6.18) and Realtek's public RTL8111 datasheets; no
//! code was taken from either. Where this driver does something else, it
//! says so ([`chip`]):
//!
//! * **ASPM and `CLKREQ#` off in the chip** (Config5, Config2, MAC OCP
//!   0xE092). Linux turns L1 off for this family unless the board vendor
//!   flagged it safe; a Ferrix driver may not write the link's control
//!   register, so the chip's own side is turned off instead.
//! * **EEE off**, in the PHY's advertisement and the MAC, where Linux
//!   turns it on.
//! * **No PHY patch.** Linux loads `rtl_nic/rtl8168h-2.fw` from
//!   linux-firmware, an EEE patch for the PHY, if it is there, and runs
//!   without it if it is not. With EEE off it has nothing to patch; whether
//!   to ship it is the customer's decision (backlog row).
//! * **The ASPM entry latency and the read request size are firmware's**:
//!   both are configuration-space writes a Ferrix driver may not make.
//!
//! # Trust
//!
//! The chip can write the rings and the receive buffers, nothing else. A
//! written-back length is checked against the buffer ([`desc::received`]);
//! the driver's own records, not the ring, say which descriptors are out;
//! and a status of all ones or a system error stops the driver, which then
//! waits for [`Driver::shutdown`].
//!
//! # Memory is given back only after a reset
//!
//! Dropping a [`Driver`] gives nothing back. [`Driver::shutdown`] stops
//! both directions and resets the chip, and only a reset seen to finish
//! returns the memory as [`Teardown::Released`].

#![no_std]
#![forbid(unsafe_code)]

#[cfg(test)]
extern crate std;

pub mod chip;
pub mod desc;
pub mod regs;

#[cfg(test)]
mod tests;

use core::fmt;

pub use chip::Chip;
use desc::{DESCRIPTOR_BYTES, DescriptorWords, Received};

/// Realtek's PCI vendor id.
pub const VENDOR: u16 = 0x10EC;

/// The 8168 family's PCI device id: the RTL8111/8168/8411 PCIe gigabit
/// adapters, of which XID tells the member apart.
pub const DEVICE: u16 = 0x8168;

/// The BAR the register window is in: BAR 2, 4 KiB of memory space (BAR 0
/// is the same registers as I/O ports, BAR 4 the MSI-X table).
pub const REGISTER_BAR: u8 = 2;

/// Bytes of the register window a driver needs mapped.
pub const REGISTER_BYTES: u64 = 0x100;

/// A page, as the device addresses in [`Pages`] count them.
pub const PAGE: usize = 4096;

/// The most descriptors one ring may have: two rings share one page.
pub const MAX_ENTRIES: usize = PAGE / 2 / DESCRIPTOR_BYTES;

/// Where in the ring page the receive ring starts: 256-byte aligned, as
/// the chip wants both.
const RX_RING_OFFSET: usize = PAGE / 2;

/// The shortest frame put on the wire, without its check sequence: a
/// shorter one is padded with zeroes by the caller's slot (see
/// [`Driver::submit`]).
pub const MIN_FRAME: u32 = 60;

/// How long the driver waits at first, and again as soon as a look moves
/// a frame, while the chip's interrupts are not arriving: 1 ms.
pub const POLL_NANOS: u64 = 1_000_000;

/// The longest the look waits while looks move nothing: 8 ms.
pub const POLL_IDLE_MAX_NANOS: u64 = 8_000_000;

/// The look's next wait after one of `prev` that `moved` frames or not:
/// back to [`POLL_NANOS`] when it moved any, else twice as long up to
/// [`POLL_IDLE_MAX_NANOS`] (backlog p2-138, as the yt6801's).
#[must_use]
pub const fn next_poll_nanos(prev: u64, moved: bool) -> u64 {
    if moved {
        return POLL_NANOS;
    }
    let doubled = prev.saturating_mul(2);
    if doubled < POLL_NANOS {
        POLL_NANOS
    } else if doubled > POLL_IDLE_MAX_NANOS {
        POLL_IDLE_MAX_NANOS
    } else {
        doubled
    }
}

/// The chip's register window: 8-, 16- and 32-bit accesses at byte
/// offsets, each one access of that width.
pub trait Registers {
    /// Read the byte at `offset`.
    fn read8(&self, offset: u32) -> u8;
    /// Read the 16-bit register at `offset`.
    fn read16(&self, offset: u32) -> u16;
    /// Read the 32-bit register at `offset`.
    fn read32(&self, offset: u32) -> u32;
    /// Write the byte at `offset`.
    fn write8(&mut self, offset: u32, value: u8);
    /// Write the 16-bit register at `offset`.
    fn write16(&mut self, offset: u32, value: u16);
    /// Write the 32-bit register at `offset`.
    fn write32(&mut self, offset: u32, value: u32);
}

/// Time, for the waits the chip's buses and resets need.
pub trait Clock {
    /// A monotonic clock, in nanoseconds.
    fn now_nanos(&self) -> u64;
    /// Let about `nanos` pass.
    fn pause(&mut self, nanos: u64);
}

/// Memory the chip reaches, by device address.
pub trait Pages {
    /// One device address per [`PAGE`], in order.
    fn device_pages(&self) -> &[u64];
}

/// Memory the chip reaches that the driver itself reads and writes: the
/// descriptor rings.
pub trait Memory: Pages {
    /// The little-endian word at `offset`, in one access.
    fn read_u32(&self, offset: usize) -> u32;
    /// Write the little-endian word at `offset`, in one access.
    fn write_u32(&mut self, offset: usize, value: u32);
    /// Order every access before this one before every access after it,
    /// for the chip as for this processor.
    fn barrier(&self);
}

/// Everything the driver is lent.
#[derive(Debug)]
pub struct Parts<R, C, M, B> {
    /// The register window.
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

/// How the driver sets the chip up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Options {
    /// Transmit descriptors, at most [`MAX_ENTRIES`].
    pub transmit_entries: u16,
    /// Receive descriptors and buffers, at most [`MAX_ENTRIES`].
    pub receive_entries: u16,
    /// Bytes in each receive buffer: a divisor of [`PAGE`] that holds a
    /// whole frame at the default MTU, so 2048 or 4096.
    pub buffer_bytes: u32,
    /// Whether 1000 Mb/s is advertised.
    pub gigabit: bool,
    /// Three bytes the made-up address takes when the chip has none.
    pub seed: [u8; 3],
}

impl Default for Options {
    fn default() -> Options {
        Options {
            transmit_entries: 64,
            receive_entries: 128,
            buffer_bytes: 2048,
            gigabit: true,
            seed: [0, 0, 1],
        }
    }
}

/// Where the station address came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacSource {
    /// The chip's permanent address (its eFuse or EEPROM, through ERI).
    Permanent,
    /// The receive filter's address as firmware left it.
    Firmware,
    /// Made up, locally administered, from [`Options::seed`].
    Generated,
}

/// What the chip and its PHY said of themselves at bring-up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Info {
    /// Which chip.
    pub chip: Chip,
    /// Its XID, from the transmit configuration.
    pub xid: u16,
    /// The PHY's identifier.
    pub phy_id: u32,
    /// The station address programmed.
    pub mac: [u8; 6],
    /// Where it came from.
    pub mac_source: MacSource,
}

/// The link, as the chip's PHY status register reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Link {
    /// No link.
    Down,
    /// A link.
    Up {
        /// Its speed.
        speed: Speed,
        /// Whether full duplex.
        full_duplex: bool,
    },
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

impl Link {
    /// The link a [`regs::PHY_STATUS`] value says.
    #[must_use]
    pub const fn from_status(status: u8) -> Link {
        if status & regs::STATUS_LINK == 0 {
            return Link::Down;
        }
        if status & regs::STATUS_1000_FULL != 0 {
            return Link::Up {
                speed: Speed::Thousand,
                full_duplex: true,
            };
        }
        let speed = if status & regs::STATUS_100 != 0 {
            Speed::Hundred
        } else {
            Speed::Ten
        };
        Link::Up {
            speed,
            full_duplex: status & regs::STATUS_FULL != 0,
        }
    }
}

/// Which wait ran out.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Timeout {
    /// An ERI access.
    Eri,
    /// A PHY OCP access.
    Phy,
    /// An EPHY access.
    Ephy,
    /// The chip's reset.
    Reset,
    /// The PHY's software reset.
    PhyReset,
    /// The shared FIFO's link list.
    LinkList,
}

/// What can go wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// The function does not answer: its registers read all ones.
    Absent,
    /// A member of the family this driver does not set up, by XID.
    Unsupported(u16),
    /// The options or the memory do not fit together.
    BadOptions,
    /// A wait ran out.
    Timeout(Timeout),
    /// The chip reported a system error, or vanished, and stopped.
    BusError,
    /// The driver failed earlier and does nothing more.
    Broken,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Absent => f.write_str("the chip does not answer (its registers read all ones)"),
            Error::Unsupported(xid) => write!(f, "XID {xid:#05x} is not an RTL8168H"),
            Error::BadOptions => f.write_str("the options and the memory do not fit"),
            Error::Timeout(which) => write!(f, "a wait ran out: {which:?}"),
            Error::BusError => f.write_str("the chip reported a system error or vanished"),
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

/// Something the chip did.
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
    /// The interrupt status bits that were set, and are now clear.
    pub status: u16,
}

/// What has happened since bring-up.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Counters {
    /// Frames sent.
    pub sent: u64,
    /// Frames received and handed on.
    pub received: u64,
    /// Receive descriptors given back with an error, a piece of a frame or
    /// a length no buffer holds, and lent again unread.
    pub receive_dropped: u64,
    /// Drains that found the chip had run out of receive descriptors.
    pub receive_unavailable: u64,
    /// Drains that found the receive FIFO had overflowed.
    pub fifo_overflows: u64,
    /// Times the transmit doorbell was rung again for frames still out.
    pub rekicks: u64,
}

/// What [`Driver::snapshot`] read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Snapshot {
    /// [`regs::CHIP_CMD`].
    pub chip_cmd: u8,
    /// [`regs::INTR_STATUS`].
    pub intr_status: u16,
    /// [`regs::INTR_MASK`].
    pub intr_mask: u16,
    /// [`regs::TX_CONFIG`].
    pub tx_config: u32,
    /// [`regs::RX_CONFIG`].
    pub rx_config: u32,
    /// [`regs::CPLUS_CMD`].
    pub cplus: u16,
    /// [`regs::MISC`].
    pub misc: u32,
    /// [`regs::MCU`].
    pub mcu: u8,
    /// [`regs::PHY_STATUS`].
    pub phy_status: u8,
    /// The station address the receive filter holds.
    pub mac: [u8; 6],
    /// The receive ring's address as the chip holds it.
    pub rx_ring: u64,
    /// The next receive descriptor the driver looks at.
    pub rx_next: usize,
    /// Its word 0.
    pub rx_word0: u32,
    /// The oldest transmit descriptor out.
    pub tx_clean: usize,
    /// Its word 0.
    pub tx_word0: u32,
}

/// A bring-up that failed, with the memory and whether the chip was seen
/// reset after the failure -- only then may the memory be freed.
#[derive(Debug)]
pub struct Failed<R, C, M, B> {
    /// Why.
    pub error: Error,
    /// What was lent.
    pub parts: Parts<R, C, M, B>,
    /// Whether the chip was reset after the failure, so it reaches no
    /// memory any more.
    pub reset: bool,
}

/// How [`Driver::shutdown`] ended.
#[derive(Debug)]
pub enum Teardown<R, C, M, B> {
    /// The chip was reset: the memory may be freed.
    Released(Parts<R, C, M, B>),
    /// The chip did not reset: the memory must stay where it can still
    /// write it.
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
    /// The next receive descriptor the chip will give back.
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

/// Whether `mac` is an address an interface may have: not all zeroes and
/// not a group address (which all ones is too).
#[must_use]
pub const fn valid_unicast(mac: [u8; 6]) -> bool {
    let zero =
        mac[0] == 0 && mac[1] == 0 && mac[2] == 0 && mac[3] == 0 && mac[4] == 0 && mac[5] == 0;
    !zero && mac[0] & 1 == 0
}

/// A locally administered unicast address from `seed`.
#[must_use]
pub const fn generated(seed: [u8; 3]) -> [u8; 6] {
    [0x02, 0x10, 0xEC, seed[0], seed[1], seed[2]]
}

impl<R: Registers, C: Clock, M: Memory, B: Pages> Driver<R, C, M, B> {
    /// Identify the chip, take it from firmware, reset it, choose and
    /// program its address, set its PHY up and start auto-negotiation, lay
    /// out the rings and start both directions. The link comes up later:
    /// see [`Driver::poll_link`].
    ///
    /// # Errors
    ///
    /// [`Failed`], with the memory back and whether the chip was reset.
    pub fn init(parts: Parts<R, C, M, B>, options: Options) -> Result<Self, Failed<R, C, M, B>> {
        let checked = Self::check(&parts, &options)
            .and_then(|rings| Ok((rings, chip::identify(parts.regs.read32(regs::TX_CONFIG))?)));
        let ((tx_ring, rx_ring), (chip, xid)) = match checked {
            Ok(found) => found,
            Err(error) => {
                return Err(Failed {
                    error,
                    parts,
                    reset: false,
                });
            }
        };
        let mut driver = Driver {
            parts,
            options,
            info: Info {
                chip,
                xid,
                phy_id: 0,
                mac: [0; 6],
                mac_source: MacSource::Generated,
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
                let reset = chip::stop(&mut parts.regs, &mut parts.clock).is_ok();
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
        let fits = (2..=MAX_ENTRIES).contains(&tx)
            && (2..=MAX_ENTRIES).contains(&rx)
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

    /// Everything after the identity, in the order the chip wants it.
    fn bring_up(&mut self) -> Result<(), Error> {
        let regs = &mut self.parts.regs;
        let clock = &mut self.parts.clock;
        let cplus = regs.read16(regs::CPLUS_CMD) & regs::CPLUS_KEEP;
        regs.write32(regs::RX_CONFIG, regs::RX_CONFIG_BASE);
        regs.write16(regs::INTR_MASK, 0);
        regs.write16(regs::INTR_STATUS, u16::MAX);
        let firmware = chip::current_mac(regs);
        chip::take_over(regs, clock)?;
        chip::reset(regs, clock)?;
        let permanent = chip::permanent_mac(regs, clock)?;
        let (mac, source) = if valid_unicast(permanent) {
            (permanent, MacSource::Permanent)
        } else if valid_unicast(firmware) {
            (firmware, MacSource::Firmware)
        } else {
            (generated(self.options.seed), MacSource::Generated)
        };
        chip::set_mac(regs, mac);
        self.info.mac = mac;
        self.info.mac_source = source;
        self.info.phy_id = chip::phy_id(regs, clock)?;
        chip::phy_setup(regs, clock, self.options.gigabit)?;
        self.arm_rings();
        chip::start(
            &mut self.parts.regs,
            &mut self.parts.clock,
            cplus,
            self.tx_ring,
            self.rx_ring,
            regs::INTERRUPTS,
        )
    }

    /// Lend the chip every receive buffer, and empty the transmit ring.
    fn arm_rings(&mut self) {
        let tx = usize::from(self.options.transmit_entries);
        for index in 0..tx {
            let end = index + 1 == tx;
            let word0 = if end { desc::RING_END } else { 0 };
            self.write_descriptor(
                index * DESCRIPTOR_BYTES,
                DescriptorWords {
                    word0,
                    word1: 0,
                    address: 0,
                },
            );
        }
        for index in 0..usize::from(self.options.receive_entries) {
            self.arm(index);
        }
    }

    /// Write a descriptor, word 0 -- which hands it over -- after the
    /// others are visible.
    fn write_descriptor(&mut self, at: usize, words: DescriptorWords) {
        let rings = &mut self.parts.rings;
        rings.write_u32(at + 4, words.word1);
        rings.write_u32(at + 8, words.address as u32);
        rings.write_u32(at + 12, (words.address >> 32) as u32);
        rings.barrier();
        rings.write_u32(at, words.word0);
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

    /// Lend receive buffer `index` to the chip.
    fn arm(&mut self, index: usize) {
        // `check` held every buffer inside the region, so there is an
        // address; a zero one is a descriptor the chip will never fill.
        let address = self.buffer_address(index).unwrap_or(0);
        let end = index + 1 == usize::from(self.options.receive_entries);
        self.write_descriptor(
            RX_RING_OFFSET + index * DESCRIPTOR_BYTES,
            desc::receive(address, self.options.buffer_bytes, end),
        );
    }

    /// The transmit doorbell.
    fn ring_transmit(&mut self) {
        self.parts.regs.write8(regs::TX_POLL, regs::POLL_NORMAL);
    }

    /// What the chip and the PHY said at bring-up.
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

    /// Read the link from the chip's PHY status register; answers the new
    /// link when it changed. The MAC follows the PHY's speed and duplex
    /// itself, so nothing is written.
    ///
    /// # Errors
    ///
    /// [`Error::Broken`] after a failure, [`Error::BusError`] if the
    /// register reads all ones.
    pub fn poll_link(&mut self) -> Result<Option<Link>, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        let status = self.parts.regs.read8(regs::PHY_STATUS);
        if status == u8::MAX && self.parts.regs.read32(regs::TX_CONFIG) == u32::MAX {
            self.broken = true;
            return Err(Error::BusError);
        }
        let now = Link::from_status(status);
        if now == self.link {
            return Ok(None);
        }
        self.link = now;
        Ok(Some(now))
    }

    /// The registers and descriptors a run on the hardware is read by:
    /// what the chip says of its own state, for a log line.
    pub fn snapshot(&mut self) -> Snapshot {
        let regs = &self.parts.regs;
        let rings = &self.parts.rings;
        Snapshot {
            chip_cmd: regs.read8(regs::CHIP_CMD),
            intr_status: regs.read16(regs::INTR_STATUS),
            intr_mask: regs.read16(regs::INTR_MASK),
            tx_config: regs.read32(regs::TX_CONFIG),
            rx_config: regs.read32(regs::RX_CONFIG),
            cplus: regs.read16(regs::CPLUS_CMD),
            misc: regs.read32(regs::MISC),
            mcu: regs.read8(regs::MCU),
            phy_status: regs.read8(regs::PHY_STATUS),
            mac: chip::current_mac(regs),
            rx_ring: u64::from(regs.read32(regs::RX_DESC_LOW))
                | (u64::from(regs.read32(regs::RX_DESC_HIGH)) << 32),
            rx_next: self.rx_next,
            rx_word0: rings.read_u32(RX_RING_OFFSET + self.rx_next * DESCRIPTOR_BYTES),
            tx_clean: self.tx_clean,
            tx_word0: rings.read_u32(self.tx_clean * DESCRIPTOR_BYTES),
        }
    }

    /// Hand `frame` to the chip to send.
    ///
    /// A frame shorter than [`MIN_FRAME`] is sent as [`MIN_FRAME`] bytes:
    /// the caller has zeroed its slot past the frame (the process's
    /// `write_transmit` does), so the padding is zeroes and never an
    /// earlier frame's bytes.
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
        let len = frame.len.max(MIN_FRAME);
        let address = device_address(
            self.parts.transmit.device_pages(),
            frame.offset,
            u64::from(len),
        )
        .ok_or(SubmitError::OutsideData)?;
        let index = self.tx_head;
        let slot = self.tx_ids.get_mut(index).ok_or(SubmitError::Broken)?;
        *slot = Some(frame.id);
        self.write_descriptor(
            index * DESCRIPTOR_BYTES,
            desc::transmit(address, len, index + 1 == entries),
        );
        self.tx_head = (index + 1) % entries;
        self.tx_out += 1;
        self.ring_transmit();
        Ok(())
    }

    /// Give receive buffer `buffer` back to the chip.
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
        Ok(())
    }

    /// Clear the interrupt status, then take what the chip finished: sent
    /// frames first, then arrived ones, as many as `out` holds.
    ///
    /// # Errors
    ///
    /// As [`Driver::drain_receiving`].
    pub fn drain(&mut self, out: &mut [Event]) -> Result<Drained, Error> {
        self.drain_receiving(out, usize::MAX)
    }

    /// [`Driver::drain`], taking at most `receive_room` arrived frames: the
    /// rest stay in the receive ring, in order, for a later call.
    ///
    /// # Errors
    ///
    /// [`Error::BusError`] on a system error or a chip that reads all
    /// ones, after which the driver does nothing more; [`Error::Broken`]
    /// after a failure.
    pub fn drain_receiving(
        &mut self,
        out: &mut [Event],
        receive_room: usize,
    ) -> Result<Drained, Error> {
        if self.broken {
            return Err(Error::Broken);
        }
        // Acknowledged before the rings are read, so a frame finished
        // after the read raises the status, and the interrupt, again.
        let status = self.parts.regs.read16(regs::INTR_STATUS);
        if status == u16::MAX {
            self.broken = true;
            return Err(Error::BusError);
        }
        if status != 0 {
            self.parts.regs.write16(regs::INTR_STATUS, status);
        }
        if status & regs::INT_SYSTEM_ERROR != 0 {
            self.broken = true;
            return Err(Error::BusError);
        }
        if status & regs::INT_RX_OVERFLOW != 0 {
            self.counters.receive_unavailable += 1;
        }
        if status & regs::INT_RX_FIFO_OVER != 0 {
            self.counters.fifo_overflows += 1;
        }
        let mut events = self.take_sent(out);
        // The 8168 can lose a doorbell rung while it was still reading the
        // descriptors before: with frames still out after a completion, it
        // is rung again (Linux does the same).
        if events > 0 && self.tx_out > 0 {
            self.counters.rekicks += 1;
            self.ring_transmit();
        }
        let room = events.saturating_add(receive_room).min(out.len());
        events += self.take_received(out.get_mut(events..room).unwrap_or_default());
        Ok(Drained { events, status })
    }

    /// Take the transmit descriptors the chip gave back, oldest first.
    fn take_sent(&mut self, out: &mut [Event]) -> usize {
        let entries = usize::from(self.options.transmit_entries);
        let mut written = 0;
        while self.tx_out > 0 && written < out.len() {
            let index = self.tx_clean;
            let word0 = self.parts.rings.read_u32(index * DESCRIPTOR_BYTES);
            if word0 & desc::OWN != 0 {
                break;
            }
            self.parts.rings.barrier();
            let id = self.tx_ids.get_mut(index).and_then(Option::take);
            self.counters.sent += 1;
            if let (Some(id), Some(slot)) = (id, out.get_mut(written)) {
                *slot = Event::Sent { id };
                written += 1;
            }
            self.tx_clean = (index + 1) % entries;
            self.tx_out -= 1;
        }
        written
    }

    /// Take the receive descriptors the chip gave back, in ring order,
    /// stopping at one it still owns or one whose buffer the caller holds.
    fn take_received(&mut self, out: &mut [Event]) -> usize {
        let entries = usize::from(self.options.receive_entries);
        let buffer = self.options.buffer_bytes;
        let mut written = 0;
        for _ in 0..entries {
            let index = self.rx_next;
            if self.rx_held.get(index).copied().unwrap_or(true) || written >= out.len() {
                break;
            }
            let at = RX_RING_OFFSET + index * DESCRIPTOR_BYTES;
            let word0 = self.parts.rings.read_u32(at);
            if word0 & desc::OWN != 0 {
                break;
            }
            self.parts.rings.barrier();
            match desc::received(word0, buffer) {
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
                }
            }
            self.rx_next = (index + 1) % entries;
        }
        written
    }

    /// Stop both directions and reset the chip.
    pub fn shutdown(self) -> Teardown<R, C, M, B> {
        let Driver { mut parts, .. } = self;
        match chip::stop(&mut parts.regs, &mut parts.clock) {
            Ok(()) => Teardown::Released(parts),
            Err(_) => Teardown::Wedged(parts),
        }
    }
}
