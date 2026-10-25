//! The AHCI driver process: a ring-3 program that serves the first SATA
//! disk of an AHCI controller to the kernel over the block ring.
//!
//! `ferrix-ahci` knows the controller and is tested on the host: it brings
//! the HBA up, finds the first port with a drive, identifies it, and moves
//! one request at a time, polled. `ferrix-driver` is the rest: START, the
//! PCI bus, pinned memory, and the block ring's conversation with the
//! kernel (`docs/BLOCK-RING.md`). What is here is the glue, as the NVMe
//! driver's is (`docs/NVME.md`): the controller's registers through BAR 5
//! (ABAR), the memory it reaches, a clock, and what is said on the console.
//!
//! # What the controller can reach
//!
//! Its command list, received-FIS area and command table, the block ring's
//! data region, and two read-ahead windows of 512 KiB: all pinned through
//! this process's device handle, so the IOMMU holds the controller to
//! exactly those pages.
//!
//! # How requests are served
//!
//! Synchronously: each request the ring hands is carried out whole before
//! `submit` returns, and answered as completed, by one `transfer` or by a
//! copy out of a read-ahead window. The controller's interrupt is bound, as
//! every PCI driver's is, and never asked for.
//!
//! Once two reads follow each other on the disk, the next 512 KiB is read
//! into a window with one command, launched as the read is answered and
//! waited for only when a request needs the drive (`Ahead`). The kernel
//! fills a file's pages 128 KiB at a time, one fill after the other; without
//! this the drive stood idle through each fill's way up and the next one's
//! way down, and a dd read 342 MB/s in QEMU where it now reads 910.
//!
//! # What is said
//!
//! One line when the function is found, before a register is written; one
//! when the drive is up, naming its model, serial and size; one line for
//! each failed request, and for each request whose command had to be issued
//! again (`ferrix_ahci::driver::ATTEMPTS`), up to a limit; one line when it
//! is shut down.
//!
//! The exit status is a `ferrix_driver::Step`, 0 a clean STOP, or a clean
//! stop before HELLO when the controller has no ATA disk (`Step::Idle`).

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::sync::atomic::{Ordering, fence};

use ferrix_ahci::{Ahci, CONTROL_PAGES, Clock, Memory, Op as AhciOp, Registers, Segment};
use ferrix_blkring::control::VMO_RIGHTS;
use ferrix_blkring::geometry::{Device as Geometry, DeviceFlags};
use ferrix_blkserve::Disk;
use ferrix_driver::dma::{Dma, PAGE};
use ferrix_driver::pci::{self, Function};
use ferrix_driver::{Driver, Step, Stopped, Stuck, block, mmio};
use ferrix_native_abi::types::DeviceInfo;
use ferrix_rt::native::handle::{Deadline, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio_blk::{
    Accepted, Completion, DeviceError, Drained, Op, Request, Status, SubmitError,
};

ferrix_rt::entry!(main);

/// The BAR an AHCI controller's registers are in: ABAR, BAR 5.
const ABAR: u8 = 5;

/// Pages of data region: what four of the largest requests read and write,
/// as the kernel's ring divides it.
const DATA_PAGES: usize = 128;

/// The most bytes one request carries: one of the data region's four parts.
const MAX_TRANSFER_BYTES: u32 = 128 * 1024;

/// The most segments one request can need: one a page, and one more for a
/// start inside a page.
const MAX_SEGMENTS: usize = MAX_TRANSFER_BYTES as usize / PAGE + 1;

/// Pages of one read-ahead window: what one command reads ahead, 512 KiB,
/// four of the kernel's largest reads.
const WINDOW_PAGES: usize = 128;

/// The most segments a window's buffer can need: one a page.
const WINDOW_SEGMENTS: usize = WINDOW_PAGES;

/// How many failed requests are said on the console before the driver
/// stops saying them.
const SAID_ERRORS: u32 = 16;

fn main(bootstrap: Bootstrap) -> i32 {
    block::run::<Sata>(bootstrap)
}

/// An AHCI controller: mass storage (`01`), SATA (`06`), AHCI 1.0 (`01`).
#[derive(Debug)]
enum AhciFunction {}

impl Function for AhciFunction {
    fn matches(info: &DeviceInfo) -> bool {
        pci::class_is(info, 0x01, 0x06, 0x01)
    }
}

type Device = pci::Device<AhciFunction>;

/// The controller's registers, through ABAR.
#[derive(Debug)]
struct Regs(mmio::Block);

impl Registers for Regs {
    fn read(&self, offset: u32) -> u32 {
        self.0.read::<u32>(offset)
    }

    fn write(&mut self, offset: u32, value: u32) {
        self.0.write::<u32>(offset, value);
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

    fn pause(&mut self, nanos: u64) {
        let until = self.now_nanos().saturating_add(nanos);
        // Nothing is ever queued on this port: the wait ends at the
        // deadline, and any other answer only shortens the pause.
        let _ = self.port.wait(Deadline::At(until));
    }
}

/// Pinned memory, as the controller logic reads and writes it.
#[derive(Debug)]
struct Pages(Dma);

impl Memory for Pages {
    fn pages(&self) -> &[u64] {
        self.0.device_pages()
    }

    fn read_u8(&self, offset: usize) -> u8 {
        self.0.read_u8(offset)
    }

    fn write_u8(&mut self, offset: usize, value: u8) {
        self.0.write_u8(offset, value);
    }

    fn read_u16(&self, offset: usize) -> u16 {
        self.0.read_u16(offset)
    }

    fn barrier(&self) {
        fence(Ordering::SeqCst);
    }
}

type Logic = Ahci<Regs, Monotonic, Pages>;

/// The controller's first drive, as the block ring serves it.
struct Sata {
    logic: Logic,
    device: Device,
    /// The data region, which the logic is handed addresses into.
    data: Dma,
    geometry: Geometry,
    /// The log2 of the drive's sector over the 512 bytes the serve loop
    /// counts in.
    scale: u32,
    data_share: Option<OwnedHandle<Kernel>>,
    errors: u32,
    /// What is read ahead of a sequential reader; none if its memory could
    /// not be pinned.
    ahead: Option<Ahead>,
}

/// One read-ahead buffer and the sectors it holds.
struct Window {
    buffer: Dma,
    /// The first drive sector it holds.
    lba: u64,
    /// How many it holds, from `lba`; none while 0.
    count: u32,
}

impl Window {
    /// Whether it holds every one of `count` sectors from `lba`.
    fn holds(&self, lba: u64, count: u32) -> bool {
        self.count > 0
            && lba >= self.lba
            && lba + u64::from(count) <= self.lba + u64::from(self.count)
    }

    /// Whether it holds any of `count` sectors from `lba`.
    fn overlaps(&self, lba: u64, count: u32) -> bool {
        self.count > 0
            && lba < self.lba + u64::from(self.count)
            && self.lba < lba + u64::from(count)
    }
}

/// Reading ahead: the kernel reads a file a fill of 128 KiB at a time, one
/// after the other, and with nothing read ahead the drive stood idle while
/// each fill went up through the kernel and the next came down. Once two
/// reads follow each other on the disk, the next window is read into one of
/// two buffers while the kernel takes what came: the drive works during the
/// kernel's part, a read it holds is a copy, and each command moves 512 KiB
/// rather than one fill. Nothing else writes the drive, so a write that
/// lands on a window is all that makes one stale, and drops it.
struct Ahead {
    windows: [Window; 2],
    /// The window the drive is filling now.
    filling: Option<usize>,
    /// Where the last read ended: a read starting there is sequential.
    last_end: u64,
}

impl fmt::Debug for Sata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sata")
            .field("geometry", &self.geometry)
            .finish_non_exhaustive()
    }
}

impl Driver for Sata {
    type Device = Device;

    fn probe(device: Device) -> Result<Self, Step> {
        let info = *device.info();
        let registers = device.map_bar(ABAR)?;
        // Said before the controller is touched, so a boot that stops in
        // the reset still says how far it came.
        ferrix_rt::info!(
            "ahci     {} {:04x}:{:04x} found: BAR 5 maps {} bytes, interrupt by {}",
            Place(info.location),
            info.vendor_id,
            info.device_id,
            registers.len(),
            if device.msix() { "MSI-X" } else { "MSI" },
        );
        said_registers(&info, &registers);
        let pinned = |pages: usize| {
            device.dma(pages).inspect_err(|_| {
                ferrix_rt::err!(
                    "ahci     {} {pages} pages could not be pinned for the controller",
                    Place(info.location)
                );
            })
        };
        let control = pinned(CONTROL_PAGES)?;
        let data = pinned(DATA_PAGES)?;
        // Reading ahead is only faster: without its memory, reads are served
        // as they come.
        let ahead = match (device.dma(WINDOW_PAGES), device.dma(WINDOW_PAGES)) {
            (Ok(first), Ok(second)) => Some(Ahead {
                windows: [
                    Window {
                        buffer: first,
                        lba: 0,
                        count: 0,
                    },
                    Window {
                        buffer: second,
                        lba: 0,
                        count: 0,
                    },
                ],
                filling: None,
                last_end: u64::MAX,
            }),
            // A window pinned and never used stays pinned, as all of the
            // controller's memory does until it stops.
            _ => None,
        };
        // HELLO's handle to the data region.
        let data_share = data.share(VMO_RIGHTS)?;
        let port = port::create(Kernel).map_err(|_| Step::Events)?;
        let logic = match Logic::init(Regs(registers), Monotonic { port }, Pages(control)) {
            Ok(logic) => logic,
            // A controller with no ATA disk behind it -- q35's built-in one
            // with only a CD-ROM, or none -- is not a failure: there is
            // nothing to serve, and every port it tried was stopped
            // (`ferrix-ahci`'s `NoDrive`).
            Err(ferrix_ahci::Error::NoDrive) => {
                ferrix_rt::warn!(
                    "ahci     {} has no ATA disk: nothing to serve",
                    Place(info.location)
                );
                return Err(Step::Idle);
            }
            Err(error) => {
                ferrix_rt::err!(
                    "ahci     {} did not come up: {error:?}",
                    Place(info.location)
                );
                // The logic is gone with its memory's handle, which stays
                // pinned: the controller is not proven stopped.
                return Err(Step::Device);
            }
        };
        let identity = logic.identity();
        let sector = identity.sector_size;
        let scale = sector.trailing_zeros().saturating_sub(9);
        let geometry = Geometry::new(
            sector,
            identity.sectors,
            MAX_TRANSFER_BYTES / sector.max(1),
            DeviceFlags::FLUSH,
            (DATA_PAGES * PAGE) as u64,
        );
        let geometry = match geometry {
            Ok(geometry) if sector.is_power_of_two() && sector >= 512 => geometry,
            _ => {
                ferrix_rt::warn!(
                    "ahci     {} sectors of {sector} bytes: not served",
                    Place(info.location)
                );
                let (_, _, control) = logic.shutdown();
                let stopped = device.stopped();
                control.0.free(&stopped);
                data.free(&stopped);
                return Err(Step::Hello);
            }
        };
        said_up(&info, &logic);
        Ok(Sata {
            logic,
            device,
            data,
            geometry,
            scale,
            data_share: Some(data_share),
            errors: 0,
            ahead,
        })
    }
}

impl Sata {
    /// Carry `request` out, whole.
    fn carry(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        let mask = (1_u64 << self.scale) - 1;
        let (op, lba, count) = match request.op {
            Op::Flush => (AhciOp::Flush, 0, 0),
            Op::Read | Op::Write => {
                if request.sector & mask != 0 || u64::from(request.count) & mask != 0 {
                    return Err(SubmitError::NotAligned);
                }
                if request.count == 0 {
                    return Err(SubmitError::Empty);
                }
                let lba = request.sector >> self.scale;
                let count = request.count >> self.scale;
                if lba
                    .checked_add(u64::from(count))
                    .is_none_or(|end| end > self.logic.identity().sectors)
                {
                    return Err(SubmitError::OutOfRange);
                }
                let op = if request.op == Op::Read {
                    AhciOp::Read
                } else {
                    AhciOp::Write
                };
                (op, lba, count)
            }
        };
        let bytes = if request.op == Op::Flush {
            0
        } else {
            request
                .count
                .checked_mul(512)
                .ok_or(SubmitError::TooLarge)?
        };
        if bytes > MAX_TRANSFER_BYTES {
            return Err(SubmitError::TooLarge);
        }
        let mut segments = [const { Segment { addr: 0, len: 0 } }; MAX_SEGMENTS];
        let used = if bytes == 0 {
            0
        } else {
            self.segments(request.data_offset, bytes, &mut segments)?
        };
        let data = segments.get(..used).unwrap_or_default();
        let hit = op == AhciOp::Read && self.serve_ahead(lba, count, request.data_offset);
        let result = if hit {
            Ok(())
        } else {
            self.settle();
            if op == AhciOp::Write {
                self.drop_ahead(lba, count);
            }
            self.logic.transfer(op, lba, count, data)
        };
        // A command issued again is said, done or not, under the same limit
        // as a failure: a drive or link that needs second tries is worth
        // knowing about before it needs more.
        if let Some((error, attempt)) = self.logic.take_retried() {
            self.errors = self.errors.saturating_add(1);
            if self.errors <= SAID_ERRORS {
                if attempt > 0 {
                    ferrix_rt::err!(
                        "ahci     request {} ({:?} of {count} sectors at {lba}) failed with \
                         {error:?} and was done on attempt {attempt}",
                        request.id,
                        request.op
                    );
                } else {
                    ferrix_rt::err!(
                        "ahci     request {} ({:?} of {count} sectors at {lba}) failed with \
                         {error:?} first, and on every attempt after",
                        request.id,
                        request.op
                    );
                }
            }
        }
        if op == AhciOp::Read && result.is_ok() {
            self.read_ahead(lba, count);
        }
        let status = match result {
            Ok(()) => Status::Ok,
            Err(error) => {
                self.errors = self.errors.saturating_add(1);
                if self.errors <= SAID_ERRORS {
                    ferrix_rt::err!(
                        "ahci     request {} ({:?} of {count} sectors at {lba}) failed: {error:?}",
                        request.id,
                        request.op
                    );
                }
                Status::IoError
            }
        };
        Ok(Accepted::Completed(Completion {
            id: request.id,
            status,
            bytes: if status == Status::Ok {
                u64::from(bytes)
            } else {
                0
            },
        }))
    }

    /// The device addresses of `bytes` of the data region from `offset`,
    /// a segment a page, pages the controller sees as contiguous merged.
    fn segments(
        &self,
        offset: u64,
        bytes: u32,
        out: &mut [Segment; MAX_SEGMENTS],
    ) -> Result<usize, SubmitError> {
        segments_of(&self.data, offset, bytes, out)
    }

    /// Serve a read of `count` sectors at `lba` from what was read ahead,
    /// into the data region at `offset`, waiting for the drive if the window
    /// it is filling holds them: whether it was.
    fn serve_ahead(&mut self, lba: u64, count: u32, offset: u64) -> bool {
        let Some(ahead) = self.ahead.as_mut() else {
            return false;
        };
        if let Some(filling) = ahead.filling
            && ahead
                .windows
                .get(filling)
                .is_some_and(|window| window.holds(lba, count))
        {
            ahead.filling = None;
            if self.logic.finish().is_err()
                && let Some(window) = ahead.windows.get_mut(filling)
            {
                window.count = 0;
            }
        }
        let size = u64::from(self.logic.identity().sector_size);
        let filling = ahead.filling;
        let Some(window) = ahead
            .windows
            .iter()
            .enumerate()
            .find(|&(index, window)| Some(index) != filling && window.holds(lba, count))
            .map(|(_, window)| window)
        else {
            return false;
        };
        let (Ok(to), Ok(at), Ok(len)) = (
            usize::try_from(offset),
            usize::try_from((lba - window.lba) * size),
            usize::try_from(u64::from(count) * size),
        ) else {
            return false;
        };
        if to
            .checked_add(len)
            .is_none_or(|end| end > DATA_PAGES * PAGE)
        {
            return false;
        }
        self.data.copy_from(to, &window.buffer, at, len);
        true
    }

    /// Wait for the window the drive is filling, before another command:
    /// kept if it read, dropped if not.
    fn settle(&mut self) {
        let Some(ahead) = self.ahead.as_mut() else {
            return;
        };
        if let Some(filling) = ahead.filling.take()
            && self.logic.finish().is_err()
            && let Some(window) = ahead.windows.get_mut(filling)
        {
            window.count = 0;
        }
    }

    /// Drop every window holding any of `count` sectors at `lba`, which a
    /// write is about to change.
    fn drop_ahead(&mut self, lba: u64, count: u32) {
        if let Some(ahead) = self.ahead.as_mut() {
            for window in &mut ahead.windows {
                if window.overlaps(lba, count) {
                    window.count = 0;
                }
            }
        }
    }

    /// After a read of `count` sectors at `lba`: if it followed the last one
    /// on the disk, and less than a window is read ahead of it, have the
    /// drive read the next window into a buffer nothing ahead is in, and
    /// return without waiting.
    fn read_ahead(&mut self, lba: u64, count: u32) {
        let sectors = self.logic.identity().sectors;
        let size = self.logic.identity().sector_size;
        let Some(ahead) = self.ahead.as_mut() else {
            return;
        };
        let end = lba + u64::from(count);
        let sequential = ahead.last_end == lba;
        ahead.last_end = end;
        if !sequential || ahead.filling.is_some() {
            return;
        }
        let per_window = (WINDOW_PAGES * PAGE) as u64 / u64::from(size.max(1));
        // The first sector from `end` not read ahead, and the windows that
        // hold the run up to it; any other window is free, whatever it
        // holds, since the reader has gone on from it or never came.
        let mut from = end;
        let mut run = [false; 2];
        while let Some(index) = ahead.windows.iter().position(|window| {
            window.count > 0 && window.lba <= from && from < window.lba + u64::from(window.count)
        }) {
            if let (Some(window), Some(used)) = (ahead.windows.get(index), run.get_mut(index)) {
                from = window.lba + u64::from(window.count);
                *used = true;
            }
        }
        let want = per_window.min(sectors.saturating_sub(from));
        if from - end >= per_window || want == 0 {
            return;
        }
        let Some(free) = run.iter().position(|&used| !used) else {
            return;
        };
        let Some(window) = ahead.windows.get_mut(free) else {
            return;
        };
        window.count = 0;
        let bytes = want * u64::from(size);
        let mut segments = [const { Segment { addr: 0, len: 0 } }; WINDOW_SEGMENTS];
        let Ok(used) = segments_of(&window.buffer, 0, bytes as u32, &mut segments) else {
            return;
        };
        let data = segments.get(..used).unwrap_or_default();
        if let Ok(read) = self.logic.start_read(from, want as u32, data) {
            window.lba = from;
            window.count = read;
            ahead.filling = Some(free);
        }
    }
}

/// The device addresses of `bytes` of `memory` from `offset`, a segment a
/// page, pages the controller sees as contiguous merged.
fn segments_of(
    memory: &Dma,
    offset: u64,
    bytes: u32,
    out: &mut [Segment],
) -> Result<usize, SubmitError> {
    let pages = memory.device_pages();
    let region = (pages.len() * PAGE) as u64;
    if offset
        .checked_add(u64::from(bytes))
        .is_none_or(|end| end > region)
    {
        return Err(SubmitError::OutsideData);
    }
    let mut at = offset as usize;
    let end = at + bytes as usize;
    let mut used = 0_usize;
    while at < end {
        let page = at / PAGE;
        let within = at % PAGE;
        let len = (PAGE - within).min(end - at);
        let addr = pages.get(page).copied().ok_or(SubmitError::OutsideData)? + within as u64;
        let merged = used > 0
            && out
                .get(used - 1)
                .is_some_and(|last| last.addr + u64::from(last.len) == addr);
        if merged {
            if let Some(last) = out.get_mut(used - 1) {
                last.len += len as u32;
            }
        } else {
            let slot = out.get_mut(used).ok_or(SubmitError::TooLarge)?;
            *slot = Segment {
                addr,
                len: len as u32,
            };
            used += 1;
        }
        at += len;
    }
    Ok(used)
}

impl Disk for Sata {
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        self.carry(request)
    }

    fn drain(&mut self, _out: &mut [Completion]) -> Result<Drained, DeviceError> {
        // Every request completed in its submit: an interrupt brings
        // nothing, and is only re-armed.
        self.device.acknowledge_interrupt();
        Ok(Drained {
            completions: 0,
            config_changed: false,
            more: false,
        })
    }
}

impl block::Device for Sata {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn data(&mut self) -> Result<OwnedHandle<Kernel>, Step> {
        self.data_share.take().ok_or(Step::Memory)
    }

    fn stop(self, _abandoned: &mut dyn FnMut(u64)) -> Result<Stopped, Stuck> {
        let Sata {
            logic,
            device,
            data,
            ahead,
            ..
        } = self;
        // Nothing the ring asked for is held between requests: nothing is
        // abandoned. A window being read ahead is waited for first.
        let (_, _, control) = logic.shutdown();
        ferrix_rt::info!("ahci     shut down: the port stopped");
        let stopped = device.stopped();
        control.0.free(&stopped);
        data.free(&stopped);
        if let Some(Ahead { windows, .. }) = ahead {
            for window in windows {
                window.buffer.free(&stopped);
            }
        }
        Ok(stopped)
    }
}

/// What firmware left in the HBA, said before it is touched: its
/// capabilities, the ports it implements, and each implemented port's link,
/// task file, signature and command register -- what decides whether a drive
/// is found, on a machine whose screen is its only output.
fn said_registers(info: &DeviceInfo, registers: &mmio::Block) {
    let read = |offset: u32| -> u32 {
        if (offset as usize) + 4 <= registers.len() {
            registers.read::<u32>(offset)
        } else {
            u32::MAX
        }
    };
    let implemented = read(0x0C);
    ferrix_rt::debug!(
        "ahci     {} CAP {:08x} GHC {:08x} PI {implemented:08x} VS {:08x} CAP2 {:08x} BOHC {:08x}",
        Place(info.location),
        read(0x00),
        read(0x04),
        read(0x10),
        read(0x24),
        read(0x28),
    );
    for index in (0..32_u32).filter(|index| implemented & (1 << index) != 0) {
        let at = 0x100 + 0x80 * index;
        ferrix_rt::debug!(
            "ahci     port {index}: SSTS {:08x} SCTL {:08x} TFD {:08x} SIG {:08x} CMD {:08x} SERR {:08x}",
            read(at + 0x28),
            read(at + 0x2C),
            read(at + 0x20),
            read(at + 0x24),
            read(at + 0x18),
            read(at + 0x30),
        );
    }
}

/// The line that says the drive is up, and what it is.
fn said_up(info: &DeviceInfo, logic: &Logic) {
    let identity = logic.identity();
    let mebibytes = identity
        .sectors
        .saturating_mul(u64::from(identity.sector_size))
        >> 20;
    ferrix_rt::info!(
        "ahci     {} {:04x}:{:04x} port {}: {} serial {}: {} sectors of {} ({mebibytes} MiB)",
        Place(info.location),
        info.vendor_id,
        info.device_id,
        identity.port,
        Text(&identity.model),
        Text(&identity.serial),
        identity.sectors,
        identity.sector_size,
    );
}

/// A PCI address, as Linux prints one.
struct Place(u32);

impl fmt::Display for Place {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let devfn = self.0 as u8;
        write!(
            f,
            "{:04x}:{:02x}:{:02x}.{}",
            self.0 >> 16,
            (self.0 >> 8) as u8,
            devfn >> 3,
            devfn & 7
        )
    }
}

/// An IDENTIFY string: ASCII, space padded, shown without its padding and
/// with anything unprintable as a dot.
struct Text<'a>(&'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = self
            .0
            .iter()
            .position(|&byte| byte != b' ' && byte != 0)
            .unwrap_or(self.0.len());
        let end = self
            .0
            .iter()
            .rposition(|&byte| byte != b' ' && byte != 0)
            .map_or(0, |last| last + 1);
        for &byte in self.0.get(start..end).unwrap_or_default() {
            let shown = if byte.is_ascii_graphic() || byte == b' ' {
                byte as char
            } else {
                '.'
            };
            f.write_char(shown)?;
        }
        Ok(())
    }
}
