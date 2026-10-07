//! The SD card driver process for an STM32MP15 board's SDMMC1: a ring-3
//! program that serves the microSD card to the kernel over the block ring.
//!
//! `ferrix-stm32-sdmmc` knows the controller and the card, and is tested on
//! the host. `ferrix-driver` is the rest: START, the device tree bus, and
//! the block ring's conversation with the kernel (`docs/BLOCK-RING.md`).
//! What is here is the glue: the controller's registers through the node's
//! mapping, a clock to time out by, the data region the kernel copies
//! through, and the card's partition table read once to decide where a
//! write may go.
//!
//! # Firmware's controller, never its DMA
//!
//! U-Boot left SDMMC1 clocked, its pins muxed and the card powered; the
//! kernel published the node without writing a register
//! (`src/kernel/src/platform/st/stm32mp1/sdmmc.rs`). This driver writes only
//! inside the controller's own window, and moves every word through the
//! FIFO: it never turns the controller's internal DMA on, and pins no
//! memory. That is this driver's property, not the kernel's: nothing in
//! front of SDMMC1 would stop a driver that did program the DMA
//! (`docs/certification/VULNERABILITY-ANALYSIS.md`, V-03).
//!
//! # Writes go only to `ferrix-` partitions
//!
//! The card holds the board's firmware and Ferrix's boot files. A write is
//! served only when every block of it lies in one GPT partition whose name
//! begins `ferrix-` (`ferrix_stm32_sdmmc::gpt`); any other is completed
//! `ReadOnly`. A card with no such partition, or whose table does not
//! check, is announced read-only, so every partition the kernel publishes
//! from it is read-only too.
//!
//! Requests are served one at a time, each completed before `submit`
//! returns: there is no queue in the controller to fill.
//!
//! The exit status is a `ferrix_driver::Step`, 0 a clean STOP.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::ptr;

use ferrix_blkring::control::VMO_RIGHTS;
use ferrix_blkring::geometry::{Device as Geometry, DeviceFlags};
use ferrix_blkserve::Disk;
use ferrix_driver::tree::{self, Binding};
use ferrix_driver::{Driver, Step, Stopped, Stuck, block};
use ferrix_native_abi::rights::Requested;
use ferrix_native_abi::types::TREE_STM32_SDMMC;
use ferrix_rt::native::handle::{Deadline, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_stm32_sdmmc::card::Card;
use ferrix_stm32_sdmmc::gpt::{Table, Writable};
use ferrix_stm32_sdmmc::host::Controller;
use ferrix_stm32_sdmmc::{BLOCK_BYTES, Clock, Registers, Words};
use ferrix_virtio_blk::{
    Accepted, Completion, DeviceError, Drained, Op, Request, Status, SubmitError,
};

ferrix_rt::entry!(main);

/// Pages of data region: four of the largest requests.
const DATA_PAGES: usize = 64;
/// Bytes of it.
const DATA_BYTES: usize = DATA_PAGES * 4096;
/// The most blocks one request moves: 64 KiB.
const MAX_SECTORS: u32 = 128;
/// How long the driver sleeps between two polls of a slow card.
const PAUSE_NANOS: u64 = 100_000;
/// How many failed requests are said on the console before the driver
/// stops saying them.
const SAID_ERRORS: u32 = 16;

fn main(bootstrap: Bootstrap) -> i32 {
    block::run::<Sdmmc>(bootstrap)
}

/// The STM32MP15's SDMMC1 node.
#[derive(Debug)]
enum Sdmmc1 {}

impl Binding for Sdmmc1 {
    const ID: u16 = TREE_STM32_SDMMC;
}

type Device = tree::Device<Sdmmc1>;

/// The controller's registers, through the node's mapping.
#[derive(Debug)]
struct Regs(Device);

impl Registers for Regs {
    fn read(&self, offset: u32) -> u32 {
        self.0.registers().read::<u32>(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        self.0.registers_mut().write::<u32>(offset, value);
    }
}

/// The monotonic clock, and a port of the driver's own to sleep on.
struct Monotonic {
    port: Port<Kernel>,
}

impl fmt::Debug for Monotonic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Monotonic").finish_non_exhaustive()
    }
}

impl Clock for Monotonic {
    fn now_nanos(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn pause(&mut self) {
        let until = self.now_nanos().saturating_add(PAUSE_NANOS);
        // Nothing is ever queued on this port: the wait ends at the
        // deadline, and any other answer only shortens the pause.
        let _ = self.port.wait(Deadline::At(until));
    }
}

/// The data region: made here, mapped here, and shared with the kernel,
/// which copies a write's bytes in before submitting it and a read's out
/// after its completion. Never pinned: no device reaches it.
struct Region {
    _vmo: Vmo<Kernel>,
    base: usize,
    len: usize,
}

impl fmt::Debug for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Region")
            .field("len", &self.len)
            .finish_non_exhaustive()
    }
}

impl Words for Region {
    fn len_bytes(&self) -> usize {
        self.len
    }

    fn read_word(&self, offset: usize) -> u32 {
        assert!(
            offset.is_multiple_of(4) && offset + 4 <= self.len,
            "a word inside the region"
        );
        // SAFETY: a mapping the kernel made for this process that lives as
        // long as `_vmo`; the offset was checked inside it and aligned;
        // volatile, since the kernel reads and writes the same pages.
        u32::from_le(unsafe { ptr::read_volatile((self.base + offset) as *const u32) })
    }

    fn write_word(&mut self, offset: usize, value: u32) {
        assert!(
            offset.is_multiple_of(4) && offset + 4 <= self.len,
            "a word inside the region"
        );
        // SAFETY: as for `read_word`, and the mapping is writable.
        unsafe { ptr::write_volatile((self.base + offset) as *mut u32, value.to_le()) }
    }
}

/// The card, as the block ring serves it.
struct Sdmmc {
    card: Card<Regs, Monotonic>,
    region: Region,
    writable: Writable,
    geometry: Geometry,
    data_share: Option<OwnedHandle<Kernel>>,
    errors: u32,
}

impl fmt::Debug for Sdmmc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sdmmc")
            .field("writable", &self.writable)
            .finish_non_exhaustive()
    }
}

impl Driver for Sdmmc {
    type Device = Device;

    fn probe(device: Device) -> Result<Self, Step> {
        let kernel_hz = device.clock_hz()?;
        let port = port::create(Kernel).map_err(|_| Step::Events)?;
        let host = Controller::take_over(Regs(device), Monotonic { port }, kernel_hz).map_err(
            |error| {
                say(format_args!(
                    "sdmmc    the controller was not taken over: {error:?}"
                ));
                Step::Device
            },
        )?;
        let mut card = Card::init(host).map_err(|(_, error)| {
            say(format_args!("sdmmc    no card came up: {error:?}"));
            Step::Device
        })?;
        let identity = *card.identity();
        let writable = writable(&mut card, identity.blocks);
        let vmo = vmo::create(Kernel, DATA_BYTES).map_err(|_| Step::Memory)?;
        let data_share = vmo
            .as_owned()
            .duplicate(Requested::Exactly(VMO_RIGHTS))
            .map_err(|_| Step::Memory)?;
        let base = vmo
            .map(None, DATA_BYTES, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        let mut flags = DeviceFlags::default();
        if writable.is_empty() {
            flags = flags.union(DeviceFlags::READ_ONLY);
        }
        let geometry = Geometry::new(
            BLOCK_BYTES as u32,
            identity.blocks,
            MAX_SECTORS,
            flags,
            DATA_BYTES as u64,
        )
        .map_err(|_| Step::Hello)?;
        say(format_args!(
            "sdmmc    card {:#06x}: {} blocks ({} MiB), {}, {} kHz, {}",
            identity.rca,
            identity.blocks,
            identity.blocks / 2048,
            if identity.high_capacity {
                "SDHC/SDXC"
            } else {
                "SDSC"
            },
            card.host().card_hz() / 1000,
            if writable.is_empty() {
                "read-only"
            } else {
                "writable in its ferrix- partitions only"
            }
        ));
        Ok(Sdmmc {
            card,
            region: Region {
                _vmo: vmo,
                base,
                len: DATA_BYTES,
            },
            writable,
            geometry,
            data_share: Some(data_share),
            errors: 0,
        })
    }
}

/// The blocks a write may touch, from the card's partition table, or none
/// when the table cannot be read or does not check.
fn writable(card: &mut Card<Regs, Monotonic>, blocks: u64) -> Writable {
    let read = |card: &mut Card<Regs, Monotonic>, number: u64| {
        card.read_block(number).map(|block| block.bytes())
    };
    let table = read(card, 1)
        .map_err(|_| "its header could not be read")
        .and_then(|header| Table::header(&header, blocks).map_err(|_| "its header is not a GPT"));
    let mut table = match table {
        Ok(table) => table,
        Err(why) => {
            say(format_args!("sdmmc    the card is read-only: {why}"));
            return Writable::NONE;
        }
    };
    for index in 0..table.entry_blocks() {
        match read(card, table.entries_at() + index) {
            Ok(block) => table.feed(&block),
            Err(_) => {
                say(format_args!(
                    "sdmmc    the card is read-only: its table could not be read"
                ));
                return Writable::NONE;
            }
        }
    }
    match table.finish() {
        Ok(writable) => {
            for &(first, last) in writable.ranges() {
                say(format_args!(
                    "sdmmc    blocks {first} to {last} are writable"
                ));
            }
            writable
        }
        Err(why) => {
            say(format_args!(
                "sdmmc    the card is read-only: its table was refused, {why:?}"
            ));
            Writable::NONE
        }
    }
}

impl Sdmmc {
    /// Say a failed request, the first few times.
    fn failed(&mut self, request: &Request, error: ferrix_stm32_sdmmc::host::Error) {
        self.errors = self.errors.saturating_add(1);
        if self.errors <= SAID_ERRORS {
            say(format_args!(
                "sdmmc    {:?} of {} blocks at {} failed: {error:?}",
                request.op, request.count, request.sector
            ));
        }
    }
}

impl Disk for Sdmmc {
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        let done = |status, bytes| {
            Ok(Accepted::Completed(Completion {
                id: request.id,
                status,
                bytes,
            }))
        };
        if request.op == Op::Flush {
            // An SD card answers a write once the block is programmed; it
            // has no volatile cache unless one is turned on, and none is.
            return done(Status::Ok, 0);
        }
        if request.count == 0 {
            return Err(SubmitError::Empty);
        }
        if request.count > MAX_SECTORS {
            return Err(SubmitError::TooLarge);
        }
        let end = request.sector.checked_add(u64::from(request.count));
        if end.is_none_or(|end| end > self.geometry.capacity()) {
            return Err(SubmitError::OutOfRange);
        }
        let bytes = u64::from(request.count) * BLOCK_BYTES as u64;
        let offset = usize::try_from(request.data_offset).map_err(|_| SubmitError::OutsideData)?;
        if request.data_offset.saturating_add(bytes) > DATA_BYTES as u64
            || !offset.is_multiple_of(4)
        {
            return Err(SubmitError::OutsideData);
        }
        let moved = match request.op {
            Op::Write => {
                if !self
                    .writable
                    .allows(request.sector, u64::from(request.count))
                {
                    return Err(SubmitError::ReadOnly);
                }
                self.card
                    .write(request.sector, request.count, &mut self.region, offset)
            }
            _ => self
                .card
                .read(request.sector, request.count, &mut self.region, offset),
        };
        match moved {
            Ok(()) => done(Status::Ok, bytes),
            Err(error) => {
                self.failed(request, error);
                done(Status::IoError, 0)
            }
        }
    }

    fn drain(&mut self, _out: &mut [Completion]) -> Result<Drained, DeviceError> {
        // Every request completed in `submit`; nothing is ever in flight.
        Ok(Drained {
            completions: 0,
            config_changed: false,
            more: false,
        })
    }
}

impl block::Device for Sdmmc {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn data(&mut self) -> Result<OwnedHandle<Kernel>, Step> {
        self.data_share.take().ok_or(Step::Memory)
    }

    fn stop(self, _abandoned: &mut dyn FnMut(u64)) -> Result<Stopped, Stuck> {
        // No request is ever in flight between two calls, so the data path
        // is idle and nothing was abandoned; and the controller was given
        // no memory to write.
        Ok(self.card.host().registers().0.stopped())
    }
}

/// A line on standard error, which a native process has open on the
/// console.
fn say(arguments: fmt::Arguments<'_>) {
    let mut line = Line::default();
    let _ = line.write_fmt(arguments);
    let _ = line.write_str("\n");
    let _ = ferrix_rt::linux::write(2, line.as_bytes());
}

/// A line, cut at its buffer's end.
struct Line {
    bytes: [u8; 160],
    len: usize,
}

impl Default for Line {
    fn default() -> Line {
        Line {
            bytes: [0; 160],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for &byte in text.as_bytes() {
            let Some(slot) = self.bytes.get_mut(self.len) else {
                return Err(fmt::Error);
            };
            *slot = byte;
            self.len += 1;
        }
        Ok(())
    }
}
