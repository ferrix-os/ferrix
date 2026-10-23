//! The STM32MP157's GPU driver process: a ring-3 program that identifies the
//! board's Vivante GC400T, brings it out of reset, and runs a command buffer
//! through its front end to an interrupt (`docs/GPU.md` §6.3, G2).
//!
//! Everything that decides a register value or a command word is
//! `ferrix-gc400`, tested on the host against models of the core. This
//! program is the handles it was written to be wrapped in:
//!
//! 1. START, as an engine's: only the device, whose common window is the
//!    core's registers. devmgr does not wait for it.
//! 2. The identity line, then the soft reset and the initialisation.
//! 3. One page for the command buffer, pinned with `PIN_COHERENT` before it
//!    is mapped: the core does not snoop the caches, so the kernel maps the
//!    page past them, and it is seen alike from both sides. One page is one
//!    run of physical addresses, which is all the front end can read with
//!    its MMU off.
//! 4. The front end started on an idle `WAIT`/`LINK` loop in that page, and
//!    two blocks spliced into the loop one after the other, each raising an
//!    event from the pixel engine. Each event is waited for as the
//!    interrupt, through a port the interrupt is bound to, and timed.
//! 5. The loop ended with an `END`, and the front end seen to go idle.
//!
//! What it finds is said on standard error, which is the console. After
//! that it stays, answering any interrupt that comes by saying what it
//! was: it publishes to no subsystem, and a driver that exited would have
//! devmgr quiesce the device and report it dead. A failure before the front
//! end starts exits with the step's number ([`Step`]); after it starts, the
//! command page may still be read by the core, so the program keeps it and
//! stays instead.

#![no_std]
#![no_main]

use core::ptr;

use ferrix_blkring::control::{Message as StartMessage, START_BYTES, Start};
use ferrix_gc400::bringup::{self, Addressing, Stuck};
use ferrix_gc400::identity::Identity;
use ferrix_gc400::regs::{common, fe, hi};
use ferrix_gc400::ring::{CommandMemory, Ring};
use ferrix_gc400::{Clock, MILLISECOND, Registers, stream};
use ferrix_native_abi::handle::Handle;
use ferrix_native_abi::signals::Signals;
use ferrix_native_abi::types::{IoMappingSpec, PACKET_INTERRUPT, TREE_STM32_GPU};
use ferrix_rt::native::channel::Channel;
use ferrix_rt::native::device::{Device, Interrupt, IoMapping};
use ferrix_rt::native::error::Error;
use ferrix_rt::native::handle::{Deadline, Object, OwnedHandle};
use ferrix_rt::native::pending::Protection;
use ferrix_rt::native::pin::{Pin, PinAccess, device_address};
use ferrix_rt::native::port::{self, Port};
use ferrix_rt::native::vmo::{self, Vmo};
use ferrix_rt::{Bootstrap, Kernel};

ferrix_rt::entry!(main);

/// A page, on every architecture this runs on.
const PAGE: usize = 4096;

/// The port key the interrupt is bound to.
const KEY_INTERRUPT: u64 = 1;

/// The events the two blocks raise. Neither is 0, so a stray bit 0 -- the
/// reset value of anything -- is not mistaken for either.
const EVENTS: [u32; 2] = [1, 2];

/// How many core cycles each `WAIT` of the idle loop lasts: etnaviv's number
/// for a core whose clock it does not know, which is this program's case.
/// At the DK board's 533 MHz it is under half a microsecond per turn.
const WAIT_CYCLES: u16 = 200;

/// How long an event may take before the front end is looked at: a block of
/// three commands takes microseconds, so a second is a hang.
const EVENT_PATIENCE: u64 = 1_000 * MILLISECOND;

/// How long the front end may take to reach the `END`.
const IDLE_PATIENCE: u64 = 100 * MILLISECOND;

/// Where a run stopped, as the exit status.
#[derive(Clone, Copy, Debug)]
#[repr(i32)]
enum Step {
    /// No bootstrap channel, or the first message was not START.
    Start = 1,
    /// START named a device that is not the STM32MP157's GPU.
    Identity = 2,
    /// The registers could not be mapped.
    Registers = 3,
    /// The command page could not be made, pinned or mapped.
    Memory = 4,
    /// The core did not come back idle from its soft reset.
    Reset = 5,
    /// The command page is somewhere the core cannot reach without an MMU.
    Addressing = 6,
    /// The port, the interrupt or the waits could not be arranged.
    Events = 7,
}

fn main(bootstrap: Bootstrap) -> i32 {
    let Some(boot) = bootstrap else {
        return Step::Start as i32;
    };
    match run(&boot) {
        Ok(never) => match never {},
        Err(step) => {
            ferrix_rt::err!("gc400: stopped at {step:?}");
            step as i32
        }
    }
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Registers, memory, time
// ---------------------------------------------------------------------------

/// The core's register page.
struct Window {
    _mapping: IoMapping<Kernel>,
    base: usize,
    len: usize,
}

impl Window {
    fn address(&self, offset: u32) -> usize {
        let offset = offset as usize;
        assert!(
            offset + 4 <= self.len && offset.is_multiple_of(4),
            "a register inside the window, aligned"
        );
        self.base + offset
    }
}

impl Registers for Window {
    fn read32(&self, offset: u32) -> u32 {
        // SAFETY: mapped device memory the kernel gave this process, the
        // offset inside it and aligned, read volatile.
        unsafe { ptr::read_volatile(self.address(offset) as *const u32) }
    }

    fn write32(&mut self, offset: u32, value: u32) {
        // SAFETY: as for `read32`, and the mapping is writable.
        unsafe { ptr::write_volatile(self.address(offset) as *mut u32, value) }
    }
}

/// The command page: pinned coherent, then mapped.
struct Page {
    _pin: Pin<Kernel>,
    _vmo: Vmo<Kernel>,
    base: usize,
    /// Its physical address, which the kernel gives as the device's.
    physical: u64,
}

impl Page {
    fn new(device: &Device<Kernel>) -> Result<Page, Step> {
        let vmo = vmo::create(Kernel, PAGE).map_err(|_| Step::Memory)?;
        // Pinned before it is mapped: a coherent pin refuses a VMO anything
        // maps, since its mappings are the ones that bypass the caches.
        let pin = device
            .pin(&vmo, 0, PAGE, PinAccess::Coherent)
            .map_err(|_| Step::Memory)?;
        let mut raw = [[0_u8; 8]; 1];
        let got = pin.addresses(&mut raw).map_err(|_| Step::Memory)?;
        if !got.is_complete() || got.pages != 1 {
            return Err(Step::Memory);
        }
        let [first] = raw;
        let base = vmo
            .map(None, PAGE, Protection::ReadWrite, 0)
            .map_err(|_| Step::Memory)?;
        Ok(Page {
            _pin: pin,
            _vmo: vmo,
            base,
            physical: device_address(first),
        })
    }

    fn at(&self, index: usize) -> usize {
        assert!(index < PAGE / 4, "a word inside the page");
        self.base + index * 4
    }
}

impl CommandMemory for Page {
    fn words(&self) -> usize {
        PAGE / 4
    }

    fn write32(&mut self, index: usize, value: u32) {
        // SAFETY: a mapping of this process's own pinned VMO that lives as
        // long as `self`; the index was checked; volatile, since the other
        // side is a device.
        unsafe { ptr::write_volatile(self.at(index) as *mut u32, value) }
    }

    fn read32(&self, index: usize) -> u32 {
        // SAFETY: as for `write32`.
        unsafe { ptr::read_volatile(self.at(index) as *const u32) }
    }

    fn barrier(&self) {
        // The core is outside the processors' shareability domain, which
        // `dmb ish` orders only within.
        ferrix_rt::device_barrier();
    }
}

/// Time: the monotonic clock, and sleeps on a port nothing is bound to.
struct Time {
    idle: Port<Kernel>,
}

impl Clock for Time {
    fn now_nanos(&self) -> u64 {
        ferrix_rt::linux::monotonic_nanos().unwrap_or(0)
    }

    fn sleep_nanos(&mut self, nanos: u64) {
        let until = self.now_nanos().saturating_add(nanos);
        let _ = self.idle.wait(Deadline::At(until));
    }
}

// ---------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------

fn started(boot: &Channel<Kernel>) -> Result<(Start, Device<Kernel>), Step> {
    let _ = boot
        .wait_one(Signals::READABLE, Deadline::Never)
        .map_err(|_| Step::Start)?;
    let mut bytes = [0_u8; START_BYTES];
    let mut handles = [Handle::INVALID; 1];
    let received = boot
        .read(&mut bytes, &mut handles)
        .map_err(|_| Step::Start)?;
    if received.handles != 1 {
        return Err(Step::Start);
    }
    let device = Device::from_owned(OwnedHandle::from_raw(Kernel, handles[0]));
    match StartMessage::decode(bytes.get(..received.bytes).unwrap_or_default()) {
        Ok(StartMessage::Start(start)) => Ok((start, device)),
        _ => Err(Step::Start),
    }
}

/// Everything the run holds once the front end may be reading the page.
struct Running {
    registers: Window,
    ring: Ring<Page>,
    port: Port<Kernel>,
    interrupt: Interrupt<Kernel>,
    clock: Time,
    _device: Device<Kernel>,
}

fn run(boot: &Channel<Kernel>) -> Result<core::convert::Infallible, Step> {
    let (start, device) = started(boot)?;
    if start.pci_device_id != TREE_STM32_GPU {
        return Err(Step::Identity);
    }
    let len = start.common.length as usize;
    if start.common.offset != 0 || len == 0 || !len.is_multiple_of(PAGE) {
        return Err(Step::Registers);
    }
    let mapping = device
        .io_mapping(IoMappingSpec {
            phys: start.common.phys,
            len: len as u64,
        })
        .map_err(|_| Step::Registers)?;
    let base = mapping.map(None).map_err(|_| Step::Registers)?;
    let mut registers = Window {
        _mapping: mapping,
        base,
        len,
    };
    let mut clock = Time {
        idle: port::create(Kernel).map_err(|_| Step::Events)?,
    };

    // Who it is, before anything is written.
    let identity = Identity::read(&registers);
    ferrix_rt::info!("gc400: {identity}");
    let features = identity.features();
    let addressing = Addressing::of(&features);
    ferrix_rt::info!(
        "gc400: {}, {} pipe, memory controller {}, addressing {addressing}",
        if identity.known().is_some() {
            "features from etnaviv's database for this core"
        } else {
            "features from the registers: etnaviv's database does not know this core"
        },
        if features.pipe_3d() { "3D" } else { "no 3D" },
        if features.mc20() { "2.0" } else { "1.0" },
    );

    let attempts = match bringup::reset(&mut registers, &mut clock, &identity) {
        Ok(attempts) => attempts,
        Err(failed) => {
            ferrix_rt::err!("gc400: the core did not reset {failed}");
            return Err(Step::Reset);
        }
    };
    bringup::init(&mut registers);
    addressing.program(&mut registers);
    let stale = registers.read32(hi::INTR_ACKNOWLEDGE);
    ferrix_rt::info!(
        "gc400: reset in {attempts} attempt(s), clock control {:#010x}, idle {:#010x}, stale interrupts {stale:#010x}",
        registers.read32(hi::CLOCK_CONTROL),
        registers.read32(hi::IDLE_STATE),
    );

    let page = Page::new(&device)?;
    let physical = page.physical;
    let Some(gpu) = addressing.gpu_address(physical) else {
        ferrix_rt::err!(
            "gc400: the command page at {physical:#x} is outside what {addressing} reaches"
        );
        return Err(Step::Addressing);
    };
    let ring = Ring::new(page, gpu, WAIT_CYCLES).map_err(|_| Step::Memory)?;
    let port = port::create(Kernel).map_err(|_| Step::Events)?;
    let interrupt = device.interrupt(0).map_err(|_| Step::Events)?;
    interrupt
        .bind(&port, KEY_INTERRUPT)
        .map_err(|_| Step::Events)?;

    let mut running = Running {
        registers,
        ring,
        port,
        interrupt,
        clock,
        _device: device,
    };
    running.exercise(physical, gpu);
    running.stay()
}

impl Running {
    /// Start the front end on the idle loop, run the two blocks, and end
    /// the loop, saying how each went. Whatever happens, the page stays.
    fn exercise(&mut self, physical: u64, gpu: u32) {
        let (address, prefetch) = self.ring.start();
        bringup::start_front_end(&mut self.registers, address, prefetch);
        self.clock.sleep_nanos(MILLISECOND);
        let debug = self.registers.read32(fe::DMA_DEBUG_STATE);
        ferrix_rt::info!(
            "gc400: command page at {physical:#x} (GPU {gpu:#010x}); front end started there, now at {:#010x} ({})",
            self.registers.read32(fe::DMA_ADDRESS),
            fe::command_state(debug),
        );

        for (index, &event) in EVENTS.iter().enumerate() {
            // The first block selects the 3D pipe, whose pixel engine raises
            // the event: nothing has selected a pipe since the reset.
            let before: &[stream::Slot] = if index == 0 {
                &[stream::pipe_select(common::PIPE_3D)]
            } else {
                &[]
            };
            let queued = self.clock.now_nanos();
            if self.ring.queue_event(before, event).is_err() {
                ferrix_rt::warn!("gc400: no room in the ring for event {event}");
                return;
            }
            if !self.await_event(event, queued) {
                return;
            }
        }

        self.ring.stop();
        let until = self.clock.now_nanos().saturating_add(IDLE_PATIENCE);
        while self.registers.read32(hi::IDLE_STATE) & hi::IDLE_STATE_FE == 0 {
            if self.clock.now_nanos() >= until {
                let stuck = Stuck::read(&self.registers);
                ferrix_rt::err!("gc400: the front end did not reach its END: {stuck}");
                return;
            }
            self.clock.sleep_nanos(MILLISECOND);
        }
        ferrix_rt::info!(
            "gc400: front end ended at {:#010x}, idle {:#010x}: command buffer ran, events {} and {} by interrupt",
            self.registers.read32(fe::DMA_ADDRESS),
            self.registers.read32(hi::IDLE_STATE),
            EVENTS[0],
            EVENTS[1],
        );
    }

    /// Wait for `event`'s interrupt, queued at `queued`: whether it came.
    /// Every interrupt on the way is acknowledged and said; a bus error or
    /// an MMU fault ends the wait, as does [`EVENT_PATIENCE`].
    fn await_event(&mut self, event: u32, queued: u64) -> bool {
        let deadline = queued.saturating_add(EVENT_PATIENCE);
        let bit = 1_u32 << event;
        loop {
            match self.port.wait(Deadline::At(deadline)) {
                Ok(packet) if packet.kind == PACKET_INTERRUPT && packet.key == KEY_INTERRUPT => {
                    // Reading acknowledges the core; the controller's line is
                    // unmasked only after, or a level interrupt would fire
                    // again at once.
                    let pending = self.registers.read32(hi::INTR_ACKNOWLEDGE);
                    let _ = self.interrupt.ack();
                    let micros = self.clock.now_nanos().saturating_sub(queued) / 1_000;
                    ferrix_rt::debug!(
                        "gc400: event {event}: interrupt with {pending:#010x} after {micros} us"
                    );
                    let faults =
                        hi::INTR_ACKNOWLEDGE_AXI_BUS_ERROR | hi::INTR_ACKNOWLEDGE_MMU_EXCEPTION;
                    if pending & faults != 0 {
                        let stuck = Stuck::read(&self.registers);
                        ferrix_rt::err!("gc400: the core faulted: {stuck}");
                        return false;
                    }
                    if pending & bit != 0 {
                        return true;
                    }
                }
                Ok(_) => {}
                Err(Error::TimedOut) => {
                    let stuck = Stuck::read(&self.registers);
                    ferrix_rt::err!(
                        "gc400: event {event} did not arrive in {} ms: {stuck}",
                        EVENT_PATIENCE / MILLISECOND
                    );
                    return false;
                }
                Err(_) => {
                    ferrix_rt::err!("gc400: the wait for event {event} failed");
                    return false;
                }
            }
        }
    }

    /// Stay, for the life of the machine: say any interrupt that comes.
    fn stay(&mut self) -> Result<core::convert::Infallible, Step> {
        loop {
            match self.port.wait(Deadline::Never) {
                Ok(packet) if packet.kind == PACKET_INTERRUPT && packet.key == KEY_INTERRUPT => {
                    let pending = self.registers.read32(hi::INTR_ACKNOWLEDGE);
                    let _ = self.interrupt.ack();
                    ferrix_rt::warn!("gc400: an interrupt nothing waited for: {pending:#010x}");
                }
                Ok(_) => {}
                Err(_) => return Err(Step::Events),
            }
        }
    }
}
