//! The NVMe driver process: a ring-3 program that serves an NVMe
//! controller's first namespace to the kernel over the block ring.
//!
//! `ferrix-nvme` knows the controller and is tested on the host.
//! `ferrix-driver` is the rest: START, the PCI bus, pinned memory, and the
//! block ring's conversation with the kernel (`docs/BLOCK-RING.md`). What
//! is here is the glue (`docs/NVME.md`): the controller's registers through
//! its first BAR, the memory it reaches, a clock, the host memory a drive
//! without DRAM asks for, and what is said on the console.
//!
//! # What the controller can reach
//!
//! Its queues, its PRP lists, the block ring's data region, and the host
//! memory buffer if it asked for one: all pinned through this process's
//! device handle, so the IOMMU holds the controller to exactly those pages.
//! None of it is freed before the controller was seen disabled; a
//! controller that will not disable keeps it, and the process exits
//! `Wedged`.
//!
//! # What is said
//!
//! One line when the function is found, before a register is written; two
//! when the controller is up, naming what it is, what was set, and the
//! fields Identify reported that decided it, so that a first boot on a
//! drive nobody has seen leaves its facts in the log; one line for each
//! abort, reset and failed request; one line when it is shut down.
//!
//! The exit status is a `ferrix_driver::Step`, 0 a clean STOP.

#![no_std]
#![no_main]

use core::fmt::{self, Write as _};
use core::sync::atomic::{Ordering, fence};

use ferrix_blkring::control::VMO_RIGHTS;
use ferrix_blkring::geometry::{Device as Geometry, DeviceFlags};
use ferrix_blkserve::Disk;
use ferrix_driver::dma::{Dma, MAX_RUN_PAGES, PAGE, Run};
use ferrix_driver::pci::{self, Function};
use ferrix_driver::{Driver, Step, Stopped, Stuck, block, mmio};
use ferrix_native_abi::types::DeviceInfo;
use ferrix_nvme::driver::{Cause, Released, list_pages_needed, queue_pages_needed};
use ferrix_nvme::hmb::{Chunk, Wish};
use ferrix_nvme::identify::Controller;
use ferrix_nvme::quirks::{self, Quirks};
use ferrix_nvme::{
    Clock, Event, Failed, Fatal, Memory, Op as NvmeOp, Options, Outcome, Parts, Registers,
    Request as NvmeRequest, Shutdown, SubmitError as NvmeSubmitError, Submitted, regs,
};
use ferrix_rt::native::handle::{Deadline, OwnedHandle};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::{Bootstrap, Kernel};
use ferrix_virtio_blk::{
    Accepted, Completion, DeviceError, Drained, Op, Request, Status, SubmitError,
};

ferrix_rt::entry!(main);

/// I/O queue pairs: one. One process serves one ring from one thread, so a
/// second pair would be a second place for the same thread to look.
const IO_QUEUES: usize = 1;

/// Pages of data region: what four of the largest requests read and write,
/// as the kernel's ring divides it.
const DATA_PAGES: usize = 128;

/// The most bytes one request carries: one of the data region's four parts.
const MAX_TRANSFER_BYTES: u32 = 128 * 1024;

/// The most pieces of host memory lent: 128 MiB in 4 MiB runs, or less in
/// smaller ones when memory is fragmented.
const HOST_MEMORY_RUNS: usize = 32;

/// The smallest run tried before lending what there is: 256 KiB.
const SMALLEST_RUN_PAGES: usize = 64;

/// How many failed requests are said on the console before the driver
/// stops saying them.
const SAID_ERRORS: u32 = 16;

fn main(bootstrap: Bootstrap) -> i32 {
    block::run::<Nvme>(bootstrap)
}

/// An NVM Express controller: mass storage, non-volatile memory, NVMe I/O
/// (class code `01:08:02`).
#[derive(Debug)]
enum Express {}

impl Function for Express {
    fn matches(info: &DeviceInfo) -> bool {
        pci::class_is(info, 0x01, 0x08, 0x02)
    }
}

type Device = pci::Device<Express>;

/// The controller's registers, through its first BAR.
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

type Made = Parts<Regs, Monotonic, Pages, Pages, Pages>;
type Logic = ferrix_nvme::Driver<Regs, Monotonic, Pages, Pages, Pages>;

/// The host memory lent to the controller.
type Lent = [Option<Run>; HOST_MEMORY_RUNS];

/// The controller, as the block ring serves it.
struct Nvme {
    logic: Logic,
    device: Device,
    lent: Lent,
    geometry: Geometry,
    /// The log2 of the namespace's block over the 512 bytes the serve loop
    /// counts in.
    scale: u32,
    data_share: Option<OwnedHandle<Kernel>>,
    errors: u32,
}

impl fmt::Debug for Nvme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Nvme")
            .field("logic", &self.logic)
            .field("geometry", &self.geometry)
            .finish_non_exhaustive()
    }
}

impl Driver for Nvme {
    type Device = Device;

    fn probe(device: Device) -> Result<Self, Step> {
        let info = *device.info();
        let quirks = quirks::for_device(info.vendor_id, info.device_id);
        if quirks.contains(Quirks::BROKEN_MSI) && !device.msix() {
            say(format_args!(
                "nvme     {} has no MSI-X and its MSI is known broken: not driven",
                Place(info.location)
            ));
            return Err(Step::Events);
        }
        let registers = device.map_bar(0)?;
        // The admin queue's doorbells are the least the BAR must hold.
        if registers.len() < (regs::cq_doorbell(0, 4) + 4) as usize {
            say(format_args!(
                "nvme     {} BAR 0 is {} bytes where it starts: too small for a controller",
                Place(info.location),
                registers.len()
            ));
            return Err(Step::Registers);
        }
        // Said before the controller is touched, so a boot that stops in
        // the reset or the enable still says how far it came.
        say(format_args!(
            "nvme     {} {:04x}:{:04x} found: BAR 0 maps {} bytes, interrupt by {}, quirks {:#x}",
            Place(info.location),
            info.vendor_id,
            info.device_id,
            registers.len(),
            if device.msix() { "MSI-X" } else { "MSI" },
            quirks.0
        ));
        let pinned = |pages: usize| {
            device.dma(pages).inspect_err(|_| {
                say(format_args!(
                    "nvme     {} {pages} pages could not be pinned for the controller",
                    Place(info.location)
                ));
            })
        };
        let queues = pinned(queue_pages_needed(IO_QUEUES))?;
        let lists = pinned(list_pages_needed(IO_QUEUES, 1))?;
        let data = pinned(DATA_PAGES)?;
        // HELLO's handle to the data region, taken before the logic owns it.
        let data_share = data.share(VMO_RIGHTS)?;
        let port = port::create(Kernel).map_err(|_| Step::Events)?;
        let register_bytes = registers.len();
        let parts = Parts {
            regs: Regs(registers),
            clock: Monotonic { port },
            queues: Pages(queues),
            lists: Pages(lists),
            data: Pages(data),
        };
        let options = Options {
            io_queues: IO_QUEUES as u16,
            quirks,
            max_transfer_bytes: MAX_TRANSFER_BYTES,
            ..Options::default()
        };
        let mut logic = match Logic::enable(parts, options) {
            Ok(logic) => logic,
            Err(failed) => return Err(refused(&device, failed)),
        };
        let mut lent: Lent = [const { None }; HOST_MEMORY_RUNS];
        let wish = logic.host_memory_wish();
        if let Some(wish) = wish {
            lend(&device, &mut logic, &wish, &mut lent);
        }
        // Every doorbell this driver writes must be inside the BAR.
        let stride = logic.capabilities().doorbell_stride();
        let last = regs::cq_doorbell(logic.io_queues() as u16, stride) + 4;
        let up = if register_bytes < last as usize {
            Err(Step::Registers)
        } else {
            logic.bring_up().map_err(|error| {
                say(format_args!(
                    "nvme     {} did not come up: {error:?}",
                    Place(info.location)
                ));
                Step::Device
            })
        };
        let geometry = up.and_then(|()| geometry(&logic));
        let geometry = match geometry {
            Ok(geometry) => geometry,
            Err(step) => {
                let _ = release(&device, logic.shutdown(), lent, &mut |_| {});
                return Err(step);
            }
        };
        said_up(&info, &logic, wish.as_ref());
        let scale = logic
            .namespace()
            .map_or(0, |namespace| u32::from(namespace.block_shift) - 9);
        Ok(Nvme {
            logic,
            device,
            lent,
            geometry,
            scale,
            data_share: Some(data_share),
            errors: 0,
        })
    }
}

/// A controller that did not come up: say why, free its memory if it was
/// seen disabled, and name the step.
fn refused(device: &Device, failed: Failed<Made>) -> Step {
    say(format_args!(
        "nvme     {} did not come up: {:?}",
        Place(device.info().location),
        failed.error
    ));
    if failed.disabled {
        let stopped = device.stopped();
        free(failed.parts, &stopped);
        Step::Device
    } else {
        // Not seen disabled: it may still reach its memory, which stays
        // pinned.
        Step::Wedged
    }
}

/// Free the queues, the lists and the data region.
fn free(parts: Made, stopped: &Stopped) {
    parts.queues.0.free(stopped);
    parts.lists.0.free(stopped);
    parts.data.0.free(stopped);
}

/// Pin host memory for the controller, in runs as large as can be had, and
/// lend it if it is enough.
fn lend(device: &Device, logic: &mut Logic, wish: &Wish, lent: &mut Lent) {
    let mut chunks = [Chunk::default(); HOST_MEMORY_RUNS];
    let mut count = 0;
    let mut total = 0_u32;
    let least = (wish.min_chunk_pages as usize).max(SMALLEST_RUN_PAGES);
    let mut size = MAX_RUN_PAGES;
    let most = wish.max_chunks.min(HOST_MEMORY_RUNS);
    while count < most && total < wish.preferred_pages && size >= least {
        let left = (wish.preferred_pages - total) as usize;
        let pages = size.min(left).max(least.min(size));
        let Ok(run) = device.run(pages) else {
            // No run this long is free: try half.
            size /= 2;
            continue;
        };
        if let (Some(chunk), Some(place)) = (chunks.get_mut(count), lent.get_mut(count)) {
            *chunk = Chunk {
                address: run.address(),
                pages: run.pages() as u32,
            };
            total += run.pages() as u32;
            *place = Some(run);
            count += 1;
        }
    }
    let chunks = chunks.get(..count).unwrap_or_default();
    // What was pinned and not lent stays pinned until the controller is
    // stopped, like everything else; it is never named to it.
    let _ = logic.lend_host_memory(chunks);
}

/// The geometry the ring announces for the namespace.
fn geometry(logic: &Logic) -> Result<Geometry, Step> {
    let namespace = logic.namespace().ok_or(Step::Device)?;
    // Every NVMe write can be forced to the medium.
    let mut flags = DeviceFlags::FUA;
    if namespace.read_only {
        flags = flags.union(DeviceFlags::READ_ONLY);
    }
    if logic.write_cache() {
        flags = flags.union(DeviceFlags::FLUSH);
    }
    Geometry::new(
        namespace.block_bytes(),
        namespace.blocks,
        logic.max_transfer_blocks(),
        flags,
        (DATA_PAGES * PAGE) as u64,
    )
    .map_err(|_| Step::Hello)
}

impl Nvme {
    /// A request of the serve loop, which counts in 512-byte sectors, in
    /// the namespace's blocks.
    fn request(&self, request: &Request, fua: bool) -> Result<NvmeRequest, SubmitError> {
        let mask = (1_u64 << self.scale) - 1;
        if request.op != Op::Flush
            && (request.sector & mask != 0 || u64::from(request.count) & mask != 0)
        {
            return Err(SubmitError::NotAligned);
        }
        Ok(NvmeRequest {
            id: request.id,
            op: match request.op {
                Op::Read => NvmeOp::Read,
                Op::Write => NvmeOp::Write,
                Op::Flush => NvmeOp::Flush,
            },
            lba: request.sector >> self.scale,
            blocks: request.count >> self.scale,
            data_offset: request.data_offset,
            fua,
        })
    }

    fn hand(&mut self, request: &Request, fua: bool) -> Result<Accepted, SubmitError> {
        let request = self.request(request, fua)?;
        match self.logic.submit(&request) {
            Ok(Submitted::Queued) => Ok(Accepted::Queued { chains: 1 }),
            Ok(Submitted::Completed(done)) => Ok(Accepted::Completed(completion(&done))),
            Err(NvmeSubmitError::Busy) => Err(SubmitError::QueueFull),
            Err(NvmeSubmitError::Dead) => Err(SubmitError::Broken),
            Err(NvmeSubmitError::Empty) => Err(SubmitError::Empty),
            Err(NvmeSubmitError::TooLarge) => Err(SubmitError::TooLarge),
            Err(NvmeSubmitError::OutOfRange) => Err(SubmitError::OutOfRange),
            Err(NvmeSubmitError::OutsideData) => Err(SubmitError::OutsideData),
            Err(NvmeSubmitError::Unaligned) => Err(SubmitError::NotAligned),
            Err(NvmeSubmitError::ReadOnly) => Err(SubmitError::ReadOnly),
        }
    }
}

/// A finished request, as the serve loop takes it.
fn completion(done: &ferrix_nvme::Completion) -> Completion {
    Completion {
        id: done.id,
        status: match done.outcome {
            Outcome::Ok => Status::Ok,
            Outcome::IoError => Status::IoError,
            Outcome::Unsupported => Status::Unsupported,
        },
        bytes: done.bytes,
    }
}

impl Disk for Nvme {
    fn submit(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        self.hand(request, false)
    }

    fn submit_fua(&mut self, request: &Request) -> Result<Accepted, SubmitError> {
        self.hand(request, true)
    }

    fn drain(&mut self, out: &mut [Completion]) -> Result<Drained, DeviceError> {
        // Re-armed first, so a completion that arrives while these are
        // taken interrupts again.
        self.device.acknowledge_interrupt();
        let mut done = [ferrix_nvme::Completion {
            id: 0,
            outcome: Outcome::Ok,
            bytes: 0,
        }; 32];
        let room = out.len().min(done.len());
        let taken = done.get_mut(..room).unwrap_or_default();
        let drained = self.logic.drain(taken).map_err(|_| DeviceError::Broken)?;
        let before = self.logic.stats().errors;
        for (place, done) in out.iter_mut().zip(taken.iter().take(drained.completions)) {
            *place = completion(done);
            if done.outcome != Outcome::Ok {
                self.errors = self.errors.saturating_add(1);
                if self.errors <= SAID_ERRORS {
                    say(format_args!(
                        "nvme     request {} failed: {:?}, last status {:#05x}, {before} failed so far",
                        done.id,
                        done.outcome,
                        self.logic.stats().last_error.0
                    ));
                }
            }
        }
        Ok(Drained {
            completions: drained.completions,
            config_changed: false,
            more: drained.more,
        })
    }

    fn busy(&self) -> bool {
        self.logic.in_flight() > 0
    }

    fn wants_drain(&self) -> bool {
        self.logic.wants_drain()
    }

    fn check(&mut self) -> Result<(), DeviceError> {
        match self.logic.check() {
            Ok(None) => Ok(()),
            Ok(Some(event)) => {
                said_event(event);
                Ok(())
            }
            Err(fatal) => {
                say(format_args!(
                    "nvme     the controller is given up: {fatal:?}"
                ));
                Err(match fatal {
                    Fatal::Gone | Fatal::Dead => DeviceError::Broken,
                    Fatal::ResetLimit | Fatal::Reset(_) => DeviceError::NeedsReset,
                })
            }
        }
    }
}

impl block::Device for Nvme {
    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn data(&mut self) -> Result<OwnedHandle<Kernel>, Step> {
        self.data_share.take().ok_or(Step::Memory)
    }

    fn stop(self, abandoned: &mut dyn FnMut(u64)) -> Result<Stopped, Stuck> {
        let Nvme {
            logic,
            device,
            lent,
            ..
        } = self;
        release(&device, logic.shutdown(), lent, abandoned)
    }
}

/// Free what a shutdown gave back, if the controller was seen disabled,
/// handing `abandoned` every request it still held.
fn release(
    device: &Device,
    shutdown: Shutdown<Made>,
    lent: Lent,
    abandoned: &mut dyn FnMut(u64),
) -> Result<Stopped, Stuck> {
    let Shutdown::Released(released) = shutdown else {
        say(format_args!(
            "nvme     the controller would not disable: its memory stays pinned"
        ));
        return Err(Stuck);
    };
    let mut held = 0_u32;
    for id in released.abandoned() {
        abandoned(id);
        held += 1;
    }
    said_down(&released, held);
    let stopped = device.stopped();
    free(released.parts, &stopped);
    for run in lent.into_iter().flatten() {
        run.free(&stopped);
    }
    Ok(stopped)
}

/// What a shutdown did, on the console.
fn said_down(released: &Released<Made>, held: u32) {
    say(format_args!(
        "nvme     shut down: {}, {held} requests abandoned",
        if released.clean {
            "the controller said its shutdown is complete"
        } else {
            "the controller did not confirm the shutdown notification"
        }
    ));
}

/// What the watch did, on the console.
fn said_event(event: Event) {
    match event {
        Event::MissedInterrupt => say(format_args!(
            "nvme     completions waited 100 ms with no interrupt: taken by the watch"
        )),
        Event::Aborted(count) => say(format_args!(
            "nvme     {count} commands timed out: abort sent"
        )),
        Event::Reset {
            cause,
            requeued,
            failed,
        } => say(format_args!(
            "nvme     controller reset ({}): {requeued} requests sent again, {failed} failed",
            match cause {
                Cause::ControllerFatal => "it reported a fatal status",
                Cause::Timeout => "a command did not come back after its abort",
            }
        )),
    }
}

/// The line that says the controller is up, and what it is.
fn said_up(info: &DeviceInfo, logic: &Logic, wish: Option<&Wish>) {
    let controller: &Controller = logic.controller();
    let (major, minor) = controller.version_pair();
    let Some(namespace) = logic.namespace() else {
        return;
    };
    let mebibytes = (namespace.blocks << namespace.block_shift) >> 20;
    let mut memory = Line::default();
    let _ = match (wish, logic.host_memory_pages()) {
        (None, _) if controller.host_memory_preferred == 0 => {
            write!(memory, "no host memory wanted")
        }
        (None, _) => write!(
            memory,
            "host memory not lent (it needs {} MiB at least)",
            controller.host_memory_minimum / 256
        ),
        (Some(wish), 0) => write!(
            memory,
            "host memory NOT in use ({} MiB wanted, {} at least): the drive runs slower",
            wish.preferred_pages / 256,
            wish.minimum_pages / 256
        ),
        (Some(wish), pages) => write!(
            memory,
            "host memory {} MiB of {} wanted",
            pages / 256,
            wish.preferred_pages / 256
        ),
    };
    say(format_args!(
        "nvme     {} {:04x}:{:04x} {} fw {} NVMe {major}.{minor}: namespace {} of {} blocks of {} \
         ({mebibytes} MiB){}, {} queue of {}, {} blocks a request, write cache {}, {}, quirks {:#x}",
        Place(info.location),
        info.vendor_id,
        info.device_id,
        Text(&controller.model),
        Text(&controller.firmware),
        namespace.id,
        namespace.blocks,
        namespace.block_bytes(),
        if namespace.read_only {
            ", read-only"
        } else {
            ""
        },
        logic.io_queues(),
        logic.entries(),
        logic.max_transfer_blocks(),
        if logic.write_cache() { "on" } else { "absent" },
        memory.as_str(),
        logic.quirks().0,
    ));
    // The fields that decided it, as the controller reported them: what a
    // person compares with `nvme id-ctrl` and `nvme show-regs`.
    let capabilities = logic.capabilities();
    say(format_args!(
        "nvme     {} said: CAP {:#018x} (ready within {} ms, {} entries a queue, doorbells {} \
         bytes apart), MDTS {}, VWC {}, APSTA {}, ACL {}, NPSS {}, RTD3E {} us, HMPRE {} HMMIN {} \
         HMMINDS {} HMMAXD {}, SQES {:#04x} CQES {:#04x}, NN {}",
        Place(info.location),
        capabilities.0,
        capabilities.ready_timeout_nanos() / 1_000_000,
        capabilities.max_queue_entries(),
        capabilities.doorbell_stride(),
        controller.max_transfer_shift,
        u8::from(controller.write_cache),
        u8::from(controller.apst),
        controller.abort_limit,
        controller.power_states,
        controller.shutdown_micros,
        controller.host_memory_preferred,
        controller.host_memory_minimum,
        controller.host_memory_min_chunk,
        controller.host_memory_max_chunks,
        controller.sq_entry_sizes,
        controller.cq_entry_sizes,
        controller.namespaces,
    ));
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

/// An Identify string: ASCII, space padded, shown without its padding and
/// with anything unprintable as a dot.
struct Text<'a>(&'a [u8]);

impl fmt::Display for Text<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = self
            .0
            .iter()
            .rposition(|&byte| byte != b' ' && byte != 0)
            .map_or(0, |last| last + 1);
        for &byte in self.0.iter().take(end) {
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
    bytes: [u8; 400],
    len: usize,
}

impl Default for Line {
    fn default() -> Line {
        Line {
            bytes: [0; 400],
            len: 0,
        }
    }
}

impl Line {
    fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or_default()
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(self.as_bytes()).unwrap_or("")
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
