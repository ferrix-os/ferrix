//! An xHCI controller, as the eXtensible Host Controller Interface
//! specification, revision 1.2, describes it: the hand-over from firmware,
//! its reset, its command and event rings, device slots, and control,
//! interrupt IN and bulk transfers. Every offset, bit, TRB type and context
//! layout below is named after the specification's table it was read from.
//!
//! # The memory
//!
//! [`area_bytes`] says how much, read from the controller's capability
//! registers before anything is written: it depends on how many scratchpad
//! pages the controller wants for itself. Nothing in the area crosses a
//! page, so the page-at-a-time addresses a pin gives are all the controller
//! needs:
//!
//! * page 0: the device context base address array, whose entry 0 points at
//! * page 1: the scratchpad buffer array;
//! * page 2: the command ring; pages 3 and 4: the event ring's segment
//!   table and its one segment;
//! * page 5: the input context every command that takes one is given --
//!   commands run one at a time;
//! * page 6: the control data buffer;
//! * then the scratchpad pages, which are the controller's alone;
//! * then a pool, a page at a time: each device slot's output context and
//!   control ring, each interrupt pipe's ring and buffers, each bulk pair's
//!   two rings and buffer.
//!
//! # One thing at a time
//!
//! A command or a control or bulk transfer is written, the doorbell rung,
//! and the event ring read in place until its event comes. Whatever else
//! the ring holds meanwhile is taken as it would be on an interrupt: an
//! interrupt pipe's packet is kept for the next [`Controller::service`], a
//! port's change is remembered for [`Host::ports_changed`], and a pipe that
//! failed is marked and recovered there, never from inside the wait -- so
//! no command is ever issued while another is waited for.
//!
//! # Bulk IN streams
//!
//! A device that answers a bulk IN request only when it has something to
//! say -- a serial adapter, whose bytes come when they come -- cannot be
//! waited for in place. [`Host::open_stream`] keeps a few requests queued
//! on such an endpoint, as an interrupt pipe's are; each completion is kept
//! in the stream until [`Host::take_chunk`] takes it, and only then is a
//! request queued in its place. A stream's page is eight buffers of
//! [`STREAM_BYTES`], of which at most four (`STREAM_QUEUED`) are ever queued or
//! waiting to be taken, so no buffer is written while its bytes are held.
//!
//! # What is not here
//!
//! Bulk streams in the specification's sense (stream IDs), isochronous
//! endpoints, power management of links and ports,
//! more than one interrupter, more than one event ring segment, and
//! `SuperSpeed` hubs. A bulk transfer moves at most a page.

use crate::host::{
    CONTROL_BYTES, Chunk, Data, Endpoint, Error, Host, PIPE_BYTES, Packet, Place, RootPort,
    RootReset, STREAM_BYTES, Target, Trouble,
};
use crate::usb::{Setup, Speed};
use crate::{Clock, Dma, MILLISECOND, Parts, Registers};

// ---------------------------------------------------------------------------
// Capability registers (Table 5-9)
// ---------------------------------------------------------------------------

/// `CAPLENGTH` in the low byte, `HCIVERSION` in the high half.
const CAPLENGTH: u32 = 0x00;
const HCSPARAMS1: u32 = 0x04;
const HCSPARAMS2: u32 = 0x08;
const HCCPARAMS1: u32 = 0x10;
const DBOFF: u32 = 0x14;
const RTSOFF: u32 = 0x18;

/// `HCCPARAMS1` (Table 5-13): 64-bit pointers.
const AC64: u32 = 1 << 0;
/// `HCCPARAMS1`: contexts are 64 bytes, not 32.
const CSZ: u32 = 1 << 2;
/// `HCCPARAMS1`: software switches port power.
const PPC: u32 = 1 << 3;

// ---------------------------------------------------------------------------
// Operational registers (Table 5-18), from `CAPLENGTH`
// ---------------------------------------------------------------------------

const USBCMD: u32 = 0x00;
const USBSTS: u32 = 0x04;
const PAGESIZE: u32 = 0x08;
const DNCTRL: u32 = 0x14;
const CRCR: u32 = 0x18;
const DCBAAP: u32 = 0x30;
const CONFIG: u32 = 0x38;
/// The first port register set; each is 16 bytes (Table 5-19).
const PORTSC: u32 = 0x400;
const PORT_STRIDE: u32 = 0x10;

// `USBCMD` (Table 5-20).
const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_INTERRUPTS: u32 = 1 << 2;
const CMD_SYSTEM_ERRORS: u32 = 1 << 3;

// `USBSTS` (Table 5-21).
const STS_HALTED: u32 = 1 << 0;
const STS_SYSTEM_ERROR: u32 = 1 << 2;
const STS_EVENT: u32 = 1 << 3;
const STS_PORT_CHANGE: u32 = 1 << 4;
const STS_NOT_READY: u32 = 1 << 11;
const STS_CONTROLLER_ERROR: u32 = 1 << 12;

/// `CRCR` (Table 5-24): the cycle state the controller starts with.
const CRCR_CYCLE: u32 = 1 << 0;

// `PORTSC` (Table 5-27).
const PORT_CONNECTED: u32 = 1 << 0;
/// Enabled. Writing a one *disables* the port, so no write carries it.
const PORT_ENABLED: u32 = 1 << 1;
const PORT_RESET: u32 = 1 << 4;
const PORT_POWER: u32 = 1 << 9;
const PORT_SPEED_SHIFT: u32 = 10;
const PORT_SPEED_MASK: u32 = 0xF;
const PORT_CONNECT_CHANGE: u32 = 1 << 17;
const PORT_ENABLE_CHANGE: u32 = 1 << 18;
const PORT_WARM_RESET_CHANGE: u32 = 1 << 19;
const PORT_OVER_CURRENT_CHANGE: u32 = 1 << 20;
const PORT_RESET_CHANGE: u32 = 1 << 21;
const PORT_LINK_CHANGE: u32 = 1 << 22;
const PORT_CONFIG_ERROR_CHANGE: u32 = 1 << 23;
const PORT_WARM_RESET: u32 = 1 << 31;
/// Every write-one-to-clear change bit.
const PORT_CHANGES: u32 = PORT_CONNECT_CHANGE
    | PORT_ENABLE_CHANGE
    | PORT_WARM_RESET_CHANGE
    | PORT_OVER_CURRENT_CHANGE
    | PORT_RESET_CHANGE
    | PORT_LINK_CHANGE
    | PORT_CONFIG_ERROR_CHANGE;
/// The bits a write must carry over as it read them: power, the indicator
/// and the three wake enables. Everything else is either read-only, a
/// strobe, or write-one-to-act.
const PORT_KEEP: u32 = PORT_POWER | (0x3 << 14) | (0x7 << 25);

// ---------------------------------------------------------------------------
// Runtime registers (Table 5-35, Table 5-37), from `RTSOFF`
// ---------------------------------------------------------------------------

/// Interrupter 0's register set.
const IR0: u32 = 0x20;
const IMAN: u32 = IR0;
const IMOD: u32 = IR0 + 0x04;
const ERSTSZ: u32 = IR0 + 0x08;
const ERSTBA: u32 = IR0 + 0x10;
const ERDP: u32 = IR0 + 0x18;

/// `IMAN` (Table 5-38): pending, write-one-to-clear.
const IMAN_PENDING: u32 = 1 << 0;
const IMAN_ENABLE: u32 = 1 << 1;
/// `IMOD`'s interval, in 250 ns: 40 microseconds, as Linux asks of a PCI
/// controller.
const MODERATION: u32 = 160;
/// `ERDP` (Table 5-42): event handler busy, write-one-to-clear.
const ERDP_BUSY: u64 = 1 << 3;

// ---------------------------------------------------------------------------
// Extended capabilities (Table 7-1, Table 7-2)
// ---------------------------------------------------------------------------

const CAPABILITY_LEGACY: u32 = 1;
const CAPABILITY_PROTOCOL: u32 = 2;
/// `USBLEGSUP` (Table 7-4).
const LEGACY_BIOS_OWNED: u32 = 1 << 16;
const LEGACY_OS_OWNED: u32 = 1 << 24;
/// `USBLEGCTLSTS` (Table 7-5): the bits a write keeps -- everything but the
/// five SMI enables (0, 4, 13, 14, 15) and the three change bits.
const LEGACY_KEEP: u32 = (0x7 << 1) | (0xFF << 5) | (0x7 << 17);
/// `USBLEGCTLSTS`: the three write-one-to-clear change bits.
const LEGACY_CHANGES: u32 = 0x7 << 29;

// ---------------------------------------------------------------------------
// TRBs (Table 6-91, Table 6-90)
// ---------------------------------------------------------------------------

const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_RESET_ENDPOINT: u32 = 14;
const TRB_STOP_ENDPOINT: u32 = 15;
const TRB_SET_DEQUEUE: u32 = 16;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;
const TRB_PORT_STATUS_CHANGE: u32 = 34;

const TRB_TYPE_SHIFT: u32 = 10;
const TRB_TYPE_MASK: u32 = 0x3F;
const TRB_CYCLE: u32 = 1 << 0;
/// A link TRB's toggle-cycle flag (Table 6-86).
const TRB_TOGGLE: u32 = 1 << 1;
/// Interrupt on a short packet (Table 6-22).
const TRB_SHORT: u32 = 1 << 2;
/// Interrupt on completion.
const TRB_IOC: u32 = 1 << 5;
/// A setup stage's eight bytes are in the TRB itself (Table 6-26).
const TRB_IMMEDIATE: u32 = 1 << 6;
/// A data or status stage towards the host (Table 6-29, Table 6-31).
const TRB_IN: u32 = 1 << 16;
/// A setup stage's transfer type (Table 6-26).
const TRT_OUT: u32 = 2 << 16;
const TRT_IN: u32 = 3 << 16;
/// Where commands and events carry a slot and an endpoint.
const TRB_SLOT_SHIFT: u32 = 24;
const TRB_ENDPOINT_SHIFT: u32 = 16;

const CODE_SUCCESS: u8 = 1;
const CODE_DATA_BUFFER: u8 = 2;
const CODE_BABBLE: u8 = 3;
const CODE_TRANSACTION: u8 = 4;
const CODE_STALL: u8 = 6;
const CODE_NO_SLOTS: u8 = 9;
const CODE_SHORT_PACKET: u8 = 13;
const CODE_STOPPED: u8 = 26;
const CODE_STOPPED_SHORT: u8 = 28;

// ---------------------------------------------------------------------------
// Contexts (section 6.2)
// ---------------------------------------------------------------------------

/// The slot context's hub flag (Table 6-4).
const SLOT_HUB: u32 = 1 << 26;
const SLOT_ENTRIES_SHIFT: u32 = 27;
const SLOT_SPEED_SHIFT: u32 = 20;
/// An endpoint context's error count, three, as the specification asks for
/// (Table 6-9).
const EP_ERRORS: u32 = 3 << 1;
const EP_TYPE_SHIFT: u32 = 3;
const EP_BULK_OUT: u32 = 2;
const EP_CONTROL: u32 = 4;
const EP_BULK_IN: u32 = 6;
const EP_INTERRUPT_IN: u32 = 7;
const EP_PACKET_SHIFT: u32 = 16;
/// The input control context's add flags for the slot and the control
/// endpoint (Table 6-16).
const ADD_SLOT: u32 = 1 << 0;
const ADD_CONTROL: u32 = 1 << 1;
/// The control endpoint's device context index.
const DCI_CONTROL: u8 = 1;

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// A page: the only size every controller supports (`PAGESIZE` bit 0).
const PAGE: usize = 4096;
/// A TRB.
const TRB_BYTES: usize = 16;
/// TRBs in a ring's one segment; the last is the link back to the first.
const RING_TRBS: usize = PAGE / TRB_BYTES;
const LINK_INDEX: usize = RING_TRBS - 1;

const DCBAA: usize = 0;
const SCRATCHPAD_ARRAY: usize = PAGE;
const COMMAND_RING: usize = 2 * PAGE;
const EVENT_TABLE: usize = 3 * PAGE;
const EVENT_RING: usize = 4 * PAGE;
const INPUT_CONTEXT: usize = 5 * PAGE;
const CONTROL_BUFFER: usize = 6 * PAGE;
/// Pages before the scratchpad pages.
const FIXED_PAGES: usize = 7;
/// The most scratchpad pages one array page names.
const MAX_SCRATCHPAD: usize = PAGE / 8;
/// Pages in the pool.
const POOL_PAGES: usize = 96;
/// The most pages [`area_bytes`] ever asks for.
pub const MAX_AREA_PAGES: usize = FIXED_PAGES + MAX_SCRATCHPAD + POOL_PAGES;

/// Device slots enabled, at most.
pub const MAX_SLOTS: usize = 32;
/// Root ports driven, at most: the bus keeps a bit a port.
pub const MAX_ROOT_PORTS: usize = 32;
/// How many interrupt pipes there may be at once.
pub const MAX_PIPES: usize = 16;
/// How many bulk pairs there may be at once.
pub const MAX_BULK: usize = 4;
/// The most a bulk transfer moves.
pub const BULK_BYTES: usize = PAGE;
/// How many bulk IN streams there may be at once.
pub const MAX_STREAMS: usize = 4;
/// Requests a stream keeps queued or holds, read, for its reader.
const STREAM_QUEUED: usize = 4;
/// Buffers a stream's page is cut into; more than [`STREAM_QUEUED`], so no
/// buffer is queued again while its bytes are held.
const STREAM_BUFFERS: usize = PAGE / STREAM_BYTES;
/// Requests an interrupt pipe keeps queued.
const QUEUED: usize = 8;
/// Buffers an interrupt pipe's page is cut into; more than [`QUEUED`], so
/// no two queued requests share one.
const PIPE_BUFFERS: usize = 16;
/// Packets kept between two calls of [`Controller::service`].
const ARRIVED: usize = 32;
/// Interrupt-pipe events kept for the console until the bus takes them.
const TROUBLES: usize = 8;

const _: () = assert!(
    PIPE_BUFFERS * PIPE_BYTES <= PAGE && QUEUED < PIPE_BUFFERS,
    "a pipe's buffers fit its page, one a queued request"
);
const _: () = assert!(
    STREAM_QUEUED < STREAM_BUFFERS,
    "a stream's buffers outnumber what it ever holds"
);
const _: () = assert!(CONTROL_BYTES <= PAGE, "the control buffer fits its page");
const _: () = assert!((MAX_SLOTS + 1) * 8 <= PAGE, "the array fits its page");
const _: () = assert!(33 * 64 <= PAGE, "an input context fits its page");

// ---------------------------------------------------------------------------
// Waits
// ---------------------------------------------------------------------------

/// How long firmware has to let go of the controller, as Linux gives it.
const HANDOVER_PATIENCE: u64 = 1000 * MILLISECOND;
/// How long a halt or a start may take: 16 ms by the specification, twice
/// that as Linux allows.
const HALT_PATIENCE: u64 = 32 * MILLISECOND;
/// How long a reset, and the controller's becoming ready after it, may take:
/// Linux's long reset wait.
const RESET_PATIENCE: u64 = 10_000 * MILLISECOND;
/// How long a command may take, as Linux allows.
const COMMAND_PATIENCE: u64 = 5000 * MILLISECOND;
/// How long a control transfer may take before it is abandoned.
const TRANSFER_PATIENCE: u64 = 1000 * MILLISECOND;
/// How long a bulk transfer may take: a disk may have to spin up.
const BULK_PATIENCE: u64 = 10_000 * MILLISECOND;
/// How long a port reset may take.
const PORT_RESET_PATIENCE: u64 = 500 * MILLISECOND;
/// The rest a device gets after its port reset: USB 2.0's `TRSTRCY`.
const RESET_RECOVERY: u64 = 10 * MILLISECOND;
/// The rest after the controller's `SET_ADDRESS`: two milliseconds by the
/// specification, ten as Linux gives.
const ADDRESS_RECOVERY: u64 = 10 * MILLISECOND;
/// How long a port's power takes to settle when the controller switches it.
const POWER_SETTLE: u64 = 20 * MILLISECOND;
/// How often a wait looks.
const POLL_STEP: u64 = MILLISECOND / 4;
/// Halts in a row after which a pipe is left stopped.
const MAX_HALTS: u32 = 4;

// ---------------------------------------------------------------------------
// What the capability registers say
// ---------------------------------------------------------------------------

/// How many scratchpad pages the controller asks for: `HCSPARAMS2`'s two
/// halves (Table 5-11), the high five bits at 25:21 and the low five at
/// 31:27.
fn scratchpad_pages(registers: &impl Registers) -> usize {
    let parameters = registers.read32(HCSPARAMS2);
    ((((parameters >> 21) & 0x1F) << 5) | ((parameters >> 27) & 0x1F)) as usize
}

/// How many bytes of memory this controller needs, read from its capability
/// registers; `None` when it wants more scratchpad pages than one array
/// page names.
#[must_use]
pub fn area_bytes(registers: &impl Registers) -> Option<usize> {
    let scratchpad = scratchpad_pages(registers);
    (scratchpad <= MAX_SCRATCHPAD).then_some((FIXED_PAGES + scratchpad + POOL_PAGES) * PAGE)
}

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

/// A ring software fills: where the next TRB goes and the cycle bit it gets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Ring {
    /// Its page's offset in the area.
    page: usize,
    enqueue: usize,
    cycle: bool,
}

/// A device slot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Slot {
    /// Its output device context's page.
    context: usize,
    /// Its control endpoint's ring.
    ring: Ring,
    /// The highest device context index in use.
    entries: u8,
}

/// An interrupt IN pipe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pipe {
    target: Target,
    /// The endpoint's number.
    number: u8,
    dci: u8,
    ring: Ring,
    /// Its buffers' page.
    buffers: usize,
    /// How many bytes each request asks for.
    length: usize,
    /// Halts in a row, reset by a good completion.
    halts: u32,
    /// Halted, and to be recovered by the next service.
    halted: bool,
}

/// A bulk pair.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Bulk {
    target: Target,
    /// The endpoints' numbers.
    number_in: u8,
    number_out: u8,
    /// Their rings.
    ring_in: Ring,
    ring_out: Ring,
    buffer: usize,
}

/// A bulk IN stream: an endpoint with requests kept queued, and what they
/// read kept until it is taken.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stream {
    target: Target,
    /// The endpoint's number.
    number: u8,
    dci: u8,
    ring: Ring,
    /// Its buffers' page.
    buffers: usize,
    /// Requests the controller holds.
    queued: usize,
    /// Each read request not yet taken, oldest first: its buffer and how
    /// many bytes it read.
    done: [Option<(usize, usize)>; STREAM_QUEUED],
    done_len: usize,
    /// Halts in a row, reset by a good completion.
    halts: u32,
    /// Halted, and to be recovered by the next service.
    halted: bool,
}

/// The transfer being waited for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Waiting {
    slot: u8,
    dci: u8,
    /// The address of its last TRB.
    last: u64,
    /// What a short data stage left unmoved.
    short: Option<u32>,
    /// How it ended: the completion code and what was left unmoved.
    done: Option<(u8, u32)>,
}

/// A command's completion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Completion {
    pointer: u64,
    code: u8,
    slot: u8,
}

/// The controller, running, and its memory.
#[derive(Debug)]
pub struct Controller<R, D, C> {
    registers: R,
    memory: D,
    clock: C,
    /// Where the operational, runtime and doorbell registers start.
    operational: u32,
    runtime: u32,
    doorbells: u32,
    ports: u32,
    /// Whether pointers are 64 bits wide.
    wide: bool,
    /// A context's size: 32 bytes, or 64.
    context_bytes: usize,
    /// Each root port's protocol -- 2, 3, or 0 for one no capability names
    /// -- and the slot type its devices' slots are asked for with.
    protocol: [u8; MAX_ROOT_PORTS],
    slot_type: [u8; MAX_ROOT_PORTS],
    /// Where the pool starts, and which of its pages are taken.
    pool: usize,
    taken: [u64; POOL_PAGES.div_ceil(64)],
    command: Ring,
    /// The event ring's next TRB and the cycle bit a new event has.
    event: usize,
    event_cycle: bool,
    slots: [Option<Slot>; MAX_SLOTS + 1],
    pipes: [Option<Pipe>; MAX_PIPES],
    bulk: [Option<Bulk>; MAX_BULK],
    streams: [Option<Stream>; MAX_STREAMS],
    /// Packets taken off the event ring and not yet handed on.
    arrived: [Option<Packet>; ARRIVED],
    arrived_head: usize,
    arrived_len: usize,
    completion: Option<Completion>,
    waiting: Option<Waiting>,
    ports_changed: bool,
    /// Interrupt-pipe events that were not a plain success, for the console.
    troubles: [Option<Trouble>; TROUBLES],
    /// Port Status Change events, for the console: the port and its status.
    port_events: [Option<(u32, u32)>; TROUBLES],
    /// The controller reported an error of its own, or stopped answering.
    dead: bool,
}

impl<R: Registers, D: Dma, C: Clock> Controller<R, D, C> {
    /// Take the controller from firmware, halt and reset it, lay its memory
    /// out, and start it with every root port powered.
    ///
    /// # Errors
    ///
    /// The error and the parts. The memory is the controller's to write for
    /// as long as it may be running: [`Error::Unresponsive`] means it may
    /// be, and the memory must then be kept.
    pub fn start(parts: Parts<R, D, C>) -> Result<Self, (Error, Parts<R, D, C>)> {
        let Parts {
            registers,
            memory,
            clock,
        } = parts;
        let operational = registers.read32(CAPLENGTH) & 0xFF;
        let structure = registers.read32(HCSPARAMS1);
        let capabilities = registers.read32(HCCPARAMS1);
        let scratchpad = scratchpad_pages(&registers);
        let mut controller = Controller {
            // Both offsets' low bits are reserved (Table 5-14, Table 5-15).
            runtime: registers.read32(RTSOFF) & !0x1F,
            doorbells: registers.read32(DBOFF) & !0x3,
            registers,
            memory,
            clock,
            operational,
            ports: (structure >> 24).min(MAX_ROOT_PORTS as u32),
            wide: capabilities & AC64 != 0,
            context_bytes: if capabilities & CSZ != 0 { 64 } else { 32 },
            protocol: [0; MAX_ROOT_PORTS],
            slot_type: [0; MAX_ROOT_PORTS],
            pool: (FIXED_PAGES + scratchpad) * PAGE,
            taken: [0; POOL_PAGES.div_ceil(64)],
            command: Ring {
                page: COMMAND_RING,
                enqueue: 0,
                cycle: true,
            },
            event: 0,
            event_cycle: true,
            slots: [None; MAX_SLOTS + 1],
            pipes: [None; MAX_PIPES],
            bulk: [None; MAX_BULK],
            streams: [None; MAX_STREAMS],
            arrived: [None; ARRIVED],
            arrived_head: 0,
            arrived_len: 0,
            completion: None,
            waiting: None,
            ports_changed: false,
            troubles: [None; TROUBLES],
            port_events: [None; TROUBLES],
            dead: false,
        };
        let slots = (structure & 0xFF).min(MAX_SLOTS as u32);
        let extended = (capabilities >> 16) << 2;
        match controller.bring_up(slots, scratchpad, extended, capabilities & PPC != 0) {
            Ok(()) => Ok(controller),
            Err(error) => Err((error, controller.into_parts())),
        }
    }

    fn bring_up(
        &mut self,
        slots: u32,
        scratchpad: usize,
        extended: u32,
        switched_power: bool,
    ) -> Result<(), Error> {
        let pages = FIXED_PAGES + scratchpad + POOL_PAGES;
        if scratchpad > MAX_SCRATCHPAD || self.memory.len() < pages * PAGE {
            return Err(Error::Memory);
        }
        // Every page where the controller can reach it, and page-aligned
        // there: a ring or a context is aligned only as far as its page is.
        for page in 0..pages {
            if !self.address(page * PAGE)?.is_multiple_of(PAGE as u64) {
                return Err(Error::Memory);
            }
        }
        self.read_capabilities(extended);

        self.halt()?;
        self.write(USBCMD, CMD_RESET);
        // A millisecond before the first look: some controllers answer
        // nothing sensible sooner.
        self.clock.sleep_nanos(MILLISECOND);
        if !self.wait_register(USBCMD, CMD_RESET, 0, RESET_PATIENCE)
            || !self.wait_register(USBSTS, STS_NOT_READY, 0, RESET_PATIENCE)
        {
            return Err(Error::Unresponsive);
        }
        // Only a controller that supports 4 KiB pages is driven.
        if self.read(PAGESIZE) & 1 == 0 {
            return Err(Error::Memory);
        }

        for offset in (0..pages * PAGE).step_by(4) {
            self.memory.write32(offset, 0);
        }
        for index in 0..scratchpad {
            let page = self.address((FIXED_PAGES + index) * PAGE)?;
            self.write_memory64(SCRATCHPAD_ARRAY + index * 8, page);
        }
        if scratchpad > 0 {
            let array = self.address(SCRATCHPAD_ARRAY)?;
            self.write_memory64(DCBAA, array);
        }
        self.link(COMMAND_RING)?;
        let segment = self.address(EVENT_RING)?;
        self.write_memory64(EVENT_TABLE, segment);
        self.memory.write32(EVENT_TABLE + 8, RING_TRBS as u32);
        self.memory.barrier();

        self.write(CONFIG, slots);
        self.write(DNCTRL, 0);
        let array = self.address(DCBAA)?;
        self.write64(self.operational + DCBAAP, array);
        let command = self.address(COMMAND_RING)?;
        self.write64(self.operational + CRCR, command | u64::from(CRCR_CYCLE));
        // The size, the dequeue pointer, and last the table's address, which
        // is what starts the ring (section 4.9.4).
        self.registers.write32(self.runtime + ERSTSZ, 1);
        self.write64(self.runtime + ERDP, segment);
        let table = self.address(EVENT_TABLE)?;
        self.write64(self.runtime + ERSTBA, table);
        self.registers.write32(self.runtime + IMOD, MODERATION);
        self.registers
            .write32(self.runtime + IMAN, IMAN_ENABLE | IMAN_PENDING);
        self.write(USBSTS, STS_EVENT | STS_PORT_CHANGE | STS_SYSTEM_ERROR);
        self.write(USBCMD, CMD_RUN | CMD_INTERRUPTS | CMD_SYSTEM_ERRORS);
        if !self.wait_register(USBSTS, STS_HALTED, 0, HALT_PATIENCE) {
            return Err(Error::Unresponsive);
        }
        if switched_power {
            for port in 0..self.ports {
                let value = self.port(port);
                self.write_port(port, (value & PORT_KEEP) | PORT_POWER);
            }
        }
        self.clock.sleep_nanos(POWER_SETTLE);
        Ok(())
    }

    /// Walk the extended capabilities: take the controller from firmware,
    /// and learn each root port's protocol.
    fn read_capabilities(&mut self, first: u32) {
        let mut at = first;
        // The list is a chain of byte-sized hops inside 64 KiB.
        for _ in 0..256 {
            if at == 0 {
                break;
            }
            let header = self.registers.read32(at);
            if header == u32::MAX {
                break;
            }
            match header & 0xFF {
                CAPABILITY_LEGACY => self.take_from_firmware(at, header),
                CAPABILITY_PROTOCOL => self.note_protocol(at, header),
                _ => {}
            }
            let next = (header >> 8) & 0xFF;
            if next == 0 {
                break;
            }
            at += next << 2;
        }
    }

    /// A supported protocol capability (Table 7-6, Table 7-8, Table 7-9):
    /// the protocol and slot type of each root port it names.
    fn note_protocol(&mut self, at: u32, header: u32) {
        let major = (header >> 24) as u8;
        let ports = self.registers.read32(at + 8);
        let slot_type = (self.registers.read32(at + 0xC) & 0x1F) as u8;
        // Port numbers count from one.
        let first = ((ports & 0xFF) as usize).saturating_sub(1);
        let count = ((ports >> 8) & 0xFF) as usize;
        for protocol in self.protocol.iter_mut().skip(first).take(count) {
            *protocol = major;
        }
        for kind in self.slot_type.iter_mut().skip(first).take(count) {
            *kind = slot_type;
        }
    }

    /// The USB legacy support capability (section 7.1): ask firmware for the
    /// controller, wait for it to let go, take it anyway if it does not --
    /// as Linux does -- and switch its SMIs off.
    fn take_from_firmware(&mut self, at: u32, header: u32) {
        if header & LEGACY_BIOS_OWNED != 0 {
            self.registers.write32(at, header | LEGACY_OS_OWNED);
            let registers = &self.registers;
            let let_go = crate::wait_for(&mut self.clock, HANDOVER_PATIENCE, MILLISECOND, || {
                registers.read32(at) & LEGACY_BIOS_OWNED == 0
            });
            if !let_go {
                let value = self.registers.read32(at);
                self.registers
                    .write32(at, (value | LEGACY_OS_OWNED) & !LEGACY_BIOS_OWNED);
            }
        }
        let control = self.registers.read32(at + 4);
        self.registers
            .write32(at + 4, (control & LEGACY_KEEP) | LEGACY_CHANGES);
    }

    /// Stop the controller and take its parts back.
    ///
    /// # Errors
    ///
    /// The parts, when the controller would not halt: it may still write
    /// the memory, which must then be kept.
    pub fn shutdown(mut self) -> Result<Parts<R, D, C>, Parts<R, D, C>> {
        let halted = self.halt().is_ok();
        if halted {
            self.write(USBCMD, CMD_RESET);
            self.clock.sleep_nanos(MILLISECOND);
            let _ = self.wait_register(USBCMD, CMD_RESET, 0, RESET_PATIENCE);
        }
        let parts = self.into_parts();
        if halted { Ok(parts) } else { Err(parts) }
    }

    fn into_parts(self) -> Parts<R, D, C> {
        Parts {
            registers: self.registers,
            memory: self.memory,
            clock: self.clock,
        }
    }

    fn halt(&mut self) -> Result<(), Error> {
        let command = self.read(USBCMD);
        self.write(USBCMD, command & !(CMD_RUN | CMD_INTERRUPTS));
        if self.wait_register(USBSTS, STS_HALTED, STS_HALTED, HALT_PATIENCE) {
            Ok(())
        } else {
            Err(Error::Unresponsive)
        }
    }

    // -----------------------------------------------------------------------
    // Root ports
    // -----------------------------------------------------------------------

    fn port(&self, port: u32) -> u32 {
        self.read(PORTSC + PORT_STRIDE * port)
    }

    fn write_port(&mut self, port: u32, value: u32) {
        self.write(PORTSC + PORT_STRIDE * port, value);
    }

    /// The speed `PORTSC` gives, by the default mapping (Table 7-13): a
    /// controller that defines speed IDs of its own is read as if it had
    /// not.
    fn port_speed(&self, port: u32, value: u32) -> Speed {
        match (value >> PORT_SPEED_SHIFT) & PORT_SPEED_MASK {
            1 => Speed::Full,
            2 => Speed::Low,
            3 => Speed::High,
            4..=7 => Speed::Super,
            _ if self.is_usb3(port) => Speed::Super,
            _ => Speed::Full,
        }
    }

    fn is_usb3(&self, port: u32) -> bool {
        self.protocol.get(port as usize) == Some(&3)
    }

    // -----------------------------------------------------------------------
    // Rings
    // -----------------------------------------------------------------------

    /// Make the page at `page` an empty ring: its last TRB links back to
    /// its first and toggles the cycle.
    fn link(&mut self, page: usize) -> Result<(), Error> {
        let first = self.address(page)?;
        let link = page + LINK_INDEX * TRB_BYTES;
        self.write_memory64(link, first);
        self.memory.write32(link + 8, 0);
        self.memory
            .write32(link + 12, (TRB_LINK << TRB_TYPE_SHIFT) | TRB_TOGGLE);
        Ok(())
    }

    /// Put one TRB on `ring`: its address. The control word goes last, with
    /// the cycle bit that hands it to the controller.
    fn enqueue(&mut self, ring: &mut Ring, parameter: u64, status: u32, control: u32) -> u64 {
        let at = ring.page + ring.enqueue * TRB_BYTES;
        let address = self.address(at).unwrap_or(0);
        self.write_memory64(at, parameter);
        self.memory.write32(at + 8, status);
        self.memory.barrier();
        self.memory
            .write32(at + 12, (control & !TRB_CYCLE) | u32::from(ring.cycle));
        ring.enqueue += 1;
        if ring.enqueue == LINK_INDEX {
            // The link goes to the controller with this lap's cycle bit, and
            // the next lap's is the other.
            let link = ring.page + LINK_INDEX * TRB_BYTES + 12;
            self.memory.write32(
                link,
                (TRB_LINK << TRB_TYPE_SHIFT) | TRB_TOGGLE | u32::from(ring.cycle),
            );
            ring.enqueue = 0;
            ring.cycle = !ring.cycle;
        }
        self.memory.barrier();
        address
    }

    /// Where `ring`'s next TRB goes, with the cycle bit it will have: what a
    /// Set TR Dequeue Pointer command and an endpoint context take.
    fn dequeue_pointer(&self, ring: &Ring) -> Result<u64, Error> {
        Ok(self.address(ring.page + ring.enqueue * TRB_BYTES)? | u64::from(ring.cycle))
    }

    fn ring_doorbell(&mut self, slot: u8, target: u8) {
        self.registers
            .write32(self.doorbells + 4 * u32::from(slot), u32::from(target));
    }

    // -----------------------------------------------------------------------
    // Events
    // -----------------------------------------------------------------------

    /// Take every event the ring holds.
    fn pump(&mut self) {
        let mut took = false;
        // A whole ring at most in one pass.
        for _ in 0..RING_TRBS {
            let at = EVENT_RING + self.event * TRB_BYTES;
            let control = self.memory.read32(at + 12);
            if (control & TRB_CYCLE != 0) != self.event_cycle {
                break;
            }
            let parameter = self.read_memory64(at);
            let status = self.memory.read32(at + 8);
            self.take_event(parameter, status, control);
            self.event += 1;
            if self.event == RING_TRBS {
                self.event = 0;
                self.event_cycle = !self.event_cycle;
            }
            took = true;
        }
        if took && let Ok(next) = self.address(EVENT_RING + self.event * TRB_BYTES) {
            self.write64(self.runtime + ERDP, next | ERDP_BUSY);
        }
    }

    fn take_event(&mut self, parameter: u64, status: u32, control: u32) {
        let code = (status >> 24) as u8;
        let slot = (control >> TRB_SLOT_SHIFT) as u8;
        match (control >> TRB_TYPE_SHIFT) & TRB_TYPE_MASK {
            TRB_COMMAND_COMPLETION => {
                self.completion = Some(Completion {
                    pointer: parameter & !0xF,
                    code,
                    slot,
                });
            }
            TRB_PORT_STATUS_CHANGE => {
                self.ports_changed = true;
                // Bits 31:24 of the parameter: the port, from one.
                if let Some(port) = (((parameter >> 24) & 0xFF) as u32).checked_sub(1) {
                    self.note_port_event(port);
                }
            }
            TRB_TRANSFER_EVENT => {
                let dci = ((control >> TRB_ENDPOINT_SHIFT) & 0x1F) as u8;
                self.take_transfer(slot, dci, parameter, code, status & 0x00FF_FFFF);
            }
            _ => {}
        }
    }

    fn take_transfer(&mut self, slot: u8, dci: u8, pointer: u64, code: u8, left: u32) {
        if let Some(waiting) = &mut self.waiting
            && waiting.slot == slot
            && waiting.dci == dci
        {
            match code {
                // A control transfer's data stage came up short: its status
                // stage is still to come.
                CODE_SHORT_PACKET if pointer != waiting.last => waiting.short = Some(left),
                CODE_SUCCESS if pointer != waiting.last => {}
                _ => waiting.done = Some((code, left)),
            }
            return;
        }
        if let Some(index) = self.streams.iter().position(|stream| {
            stream.is_some_and(|stream| stream.target.slot == slot && stream.dci == dci)
        }) {
            self.take_stream_transfer(index, pointer, code, left);
            return;
        }
        let Some(index) = self
            .pipes
            .iter()
            .position(|pipe| pipe.is_some_and(|pipe| pipe.target.slot == slot && pipe.dci == dci))
        else {
            return;
        };
        let Some(Some(mut pipe)) = self.pipes.get(index).copied() else {
            return;
        };
        let known = self.buffer_of(&pipe, pointer).is_some();
        if code != CODE_SUCCESS || !known {
            self.note_trouble(Trouble {
                slot,
                endpoint: dci,
                code,
                known,
            });
        }
        match code {
            CODE_SUCCESS | CODE_SHORT_PACKET => {
                let Some(buffer) = self.buffer_of(&pipe, pointer) else {
                    // An event for a TRB this did not queue -- what a
                    // controller's own idea of the ring says. The request it
                    // ended is replaced all the same, so the pipe does not run
                    // dry, and its bytes are not trusted.
                    self.queue_request(&mut pipe);
                    self.ring_doorbell(slot, dci);
                    if let Some(entry) = self.pipes.get_mut(index) {
                        *entry = Some(pipe);
                    }
                    return;
                };
                let len = pipe.length.saturating_sub(left as usize).min(PIPE_BYTES);
                let mut packet = Packet {
                    pipe: index,
                    bytes: [0; PIPE_BYTES],
                    len,
                };
                for (at, slot) in packet.bytes.iter_mut().enumerate().take(len) {
                    *slot = self.memory.read8(buffer + at);
                }
                self.keep(packet);
                pipe.halts = 0;
                self.queue_request(&mut pipe);
                self.ring_doorbell(slot, dci);
            }
            // What a Stop Endpoint command leaves behind.
            CODE_STOPPED..=CODE_STOPPED_SHORT => {}
            _ => pipe.halted = true,
        }
        if let Some(entry) = self.pipes.get_mut(index) {
            *entry = Some(pipe);
        }
    }

    /// A stream's request ended: what it read is kept for the reader, or
    /// the stream is marked for recovery.
    fn take_stream_transfer(&mut self, index: usize, pointer: u64, code: u8, left: u32) {
        let Some(Some(mut stream)) = self.streams.get(index).copied() else {
            return;
        };
        let buffer = self.stream_buffer_of(&stream, pointer);
        if (code != CODE_SUCCESS && code != CODE_SHORT_PACKET) || buffer.is_none() {
            self.note_trouble(Trouble {
                slot: stream.target.slot,
                endpoint: stream.dci,
                code,
                known: buffer.is_some(),
            });
        }
        match code {
            CODE_SUCCESS | CODE_SHORT_PACKET => {
                stream.queued = stream.queued.saturating_sub(1);
                stream.halts = 0;
                let len = STREAM_BYTES.saturating_sub(left as usize);
                match buffer {
                    Some(buffer) if stream.done_len < STREAM_QUEUED => {
                        if let Some(slot) = stream.done.get_mut(stream.done_len) {
                            *slot = Some((buffer, len));
                            stream.done_len += 1;
                        }
                    }
                    // Not a request this queued: its bytes are not
                    // trusted, and the request it ended is replaced.
                    _ => {
                        self.queue_stream_request(&mut stream);
                        self.ring_doorbell(stream.target.slot, stream.dci);
                    }
                }
            }
            CODE_STOPPED..=CODE_STOPPED_SHORT => {}
            _ => stream.halted = true,
        }
        if let Some(entry) = self.streams.get_mut(index) {
            *entry = Some(stream);
        }
    }

    /// The buffer of the request of `stream` whose TRB is at `pointer`.
    fn stream_buffer_of(&self, stream: &Stream, pointer: u64) -> Option<usize> {
        let base = self.address(stream.ring.page).ok()?;
        let offset = usize::try_from(pointer.checked_sub(base)?).ok()?;
        if offset >= LINK_INDEX * TRB_BYTES || !offset.is_multiple_of(TRB_BYTES) {
            return None;
        }
        Some(stream.buffers + (offset / TRB_BYTES % STREAM_BUFFERS) * STREAM_BYTES)
    }

    /// Queue one more request on a stream.
    fn queue_stream_request(&mut self, stream: &mut Stream) {
        let buffer = stream.buffers + (stream.ring.enqueue % STREAM_BUFFERS) * STREAM_BYTES;
        let Ok(address) = self.address(buffer) else {
            return;
        };
        let _ = self.enqueue(
            &mut stream.ring,
            address,
            STREAM_BYTES as u32,
            (TRB_NORMAL << TRB_TYPE_SHIFT) | TRB_IOC | TRB_SHORT,
        );
        stream.queued += 1;
    }

    /// Recover every stream an event marked halted, as
    /// [`Self::recover_pipes`] does a pipe: what it read and was not taken
    /// is kept, and as many requests queued again as leave room for it.
    fn recover_streams(&mut self) {
        for index in 0..MAX_STREAMS {
            let Some(Some(mut stream)) = self.streams.get(index).copied() else {
                continue;
            };
            if !stream.halted {
                continue;
            }
            stream.halted = false;
            stream.halts += 1;
            stream.queued = 0;
            if stream.halts <= MAX_HALTS {
                let slot = stream.target.slot;
                self.restart_endpoint(slot, stream.dci, &stream.ring, true);
                self.clear_halt(&stream.target, stream.number | 0x80);
                while stream.queued + stream.done_len < STREAM_QUEUED {
                    self.queue_stream_request(&mut stream);
                }
                self.ring_doorbell(slot, stream.dci);
            }
            if let Some(entry) = self.streams.get_mut(index) {
                *entry = Some(stream);
            }
        }
    }

    /// Keep root port `port`'s status for the console, as a Port Status
    /// Change event found it; the oldest goes when there is no room.
    fn note_port_event(&mut self, port: u32) {
        let status = self.port(port);
        if self.port_events.iter().all(Option::is_some) {
            self.port_events.rotate_left(1);
            if let Some(last) = self.port_events.last_mut() {
                *last = None;
            }
        }
        if let Some(slot) = self.port_events.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some((port, status));
        }
    }

    /// Keep `trouble` for the console; the oldest goes when there is no room.
    fn note_trouble(&mut self, trouble: Trouble) {
        if self.troubles.iter().all(Option::is_some) {
            self.troubles.rotate_left(1);
            if let Some(last) = self.troubles.last_mut() {
                *last = None;
            }
        }
        if let Some(slot) = self.troubles.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(trouble);
        }
    }

    /// The buffer of the request of `pipe` whose TRB is at `pointer`.
    fn buffer_of(&self, pipe: &Pipe, pointer: u64) -> Option<usize> {
        let base = self.address(pipe.ring.page).ok()?;
        let offset = usize::try_from(pointer.checked_sub(base)?).ok()?;
        if offset >= LINK_INDEX * TRB_BYTES || !offset.is_multiple_of(TRB_BYTES) {
            return None;
        }
        Some(pipe.buffers + (offset / TRB_BYTES % PIPE_BUFFERS) * PIPE_BYTES)
    }

    /// Queue one more request on an interrupt pipe.
    fn queue_request(&mut self, pipe: &mut Pipe) {
        let buffer = pipe.buffers + (pipe.ring.enqueue % PIPE_BUFFERS) * PIPE_BYTES;
        let Ok(address) = self.address(buffer) else {
            return;
        };
        let _ = self.enqueue(
            &mut pipe.ring,
            address,
            pipe.length as u32,
            (TRB_NORMAL << TRB_TYPE_SHIFT) | TRB_IOC | TRB_SHORT,
        );
    }

    fn keep(&mut self, packet: Packet) {
        if self.arrived_len == ARRIVED {
            // The oldest goes: a reader this far behind has lost it anyway.
            self.arrived_head = (self.arrived_head + 1) % ARRIVED;
            self.arrived_len -= 1;
        }
        if let Some(slot) = self
            .arrived
            .get_mut((self.arrived_head + self.arrived_len) % ARRIVED)
        {
            *slot = Some(packet);
            self.arrived_len += 1;
        }
    }

    /// Read events until `done` says so, for at most `limit`.
    fn wait_events(&mut self, limit: u64, done: impl Fn(&Self) -> bool) -> bool {
        let until = self.clock.now_nanos().saturating_add(limit);
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            let status = self.read(USBSTS);
            if status & (STS_SYSTEM_ERROR | STS_CONTROLLER_ERROR) != 0 {
                self.dead = true;
                return false;
            }
            if self.clock.now_nanos() >= until {
                return false;
            }
            self.clock.sleep_nanos(POLL_STEP);
        }
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    /// Run one command to completion.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`] from a controller that never answered, which is
    /// then taken for dead; [`Error::Command`] with any code but success.
    fn command(&mut self, parameter: u64, control: u32) -> Result<Completion, Error> {
        if self.dead {
            return Err(Error::Unresponsive);
        }
        let mut ring = self.command;
        let pointer = self.enqueue(&mut ring, parameter, 0, control);
        self.command = ring;
        self.completion = None;
        self.ring_doorbell(0, 0);
        let answered = self.wait_events(COMMAND_PATIENCE, |controller| {
            controller
                .completion
                .is_some_and(|completion| completion.pointer == pointer)
        });
        let Some(completion) = self.completion.take().filter(|_| answered) else {
            self.dead = true;
            return Err(Error::Timeout);
        };
        match completion.code {
            CODE_SUCCESS => Ok(completion),
            CODE_NO_SLOTS => Err(Error::NoSlot),
            CODE_TRANSACTION => Err(Error::Transaction),
            code => Err(Error::Command(code)),
        }
    }

    fn slot_command(&mut self, kind: u32, slot: u8, parameter: u64) -> Result<Completion, Error> {
        self.command(
            parameter,
            (kind << TRB_TYPE_SHIFT) | (u32::from(slot) << TRB_SLOT_SHIFT),
        )
    }

    fn endpoint_command(
        &mut self,
        kind: u32,
        slot: u8,
        dci: u8,
        parameter: u64,
    ) -> Result<Completion, Error> {
        self.command(
            parameter,
            (kind << TRB_TYPE_SHIFT)
                | (u32::from(slot) << TRB_SLOT_SHIFT)
                | (u32::from(dci) << TRB_ENDPOINT_SHIFT),
        )
    }

    /// Bring a halted or stopped endpoint back with an empty ring: Reset
    /// Endpoint if it `halted`, then its dequeue pointer set to where the
    /// next TRB will go.
    fn restart_endpoint(&mut self, slot: u8, dci: u8, ring: &Ring, halted: bool) {
        if halted {
            let _ = self.endpoint_command(TRB_RESET_ENDPOINT, slot, dci, 0);
        }
        if let Ok(pointer) = self.dequeue_pointer(ring) {
            let _ = self.endpoint_command(TRB_SET_DEQUEUE, slot, dci, pointer);
        }
    }

    // -----------------------------------------------------------------------
    // Contexts
    // -----------------------------------------------------------------------

    /// The offset of word `word` of context `index` in the context page at
    /// `page`.
    const fn context(&self, page: usize, index: usize, word: usize) -> usize {
        page + index * self.context_bytes + word * 4
    }

    /// Start an input context: every word zero, the add and drop flags set,
    /// and -- when `slot` is given -- the slot context copied from the
    /// slot's output context with `entries` context entries.
    fn begin_input(&mut self, add: u32, drop: u32, slot: Option<(&Slot, u8)>) {
        for offset in (0..33 * self.context_bytes).step_by(4) {
            self.memory.write32(INPUT_CONTEXT + offset, 0);
        }
        self.memory.write32(self.context(INPUT_CONTEXT, 0, 0), drop);
        self.memory.write32(self.context(INPUT_CONTEXT, 0, 1), add);
        if let Some((slot, entries)) = slot {
            for word in 0..3 {
                let value = self.memory.read32(self.context(slot.context, 0, word));
                self.memory
                    .write32(self.context(INPUT_CONTEXT, 1, word), value);
            }
            let first = self.context(INPUT_CONTEXT, 1, 0);
            let value = self.memory.read32(first) & !(0x1F << SLOT_ENTRIES_SHIFT);
            self.memory
                .write32(first, value | (u32::from(entries) << SLOT_ENTRIES_SHIFT));
        }
    }

    /// Write the input context's endpoint context for `dci`.
    fn input_endpoint(
        &mut self,
        dci: u8,
        kind: u32,
        max_packet: u16,
        interval: u32,
        ring: &Ring,
        average: u32,
    ) -> Result<(), Error> {
        let index = usize::from(dci) + 1;
        let pointer = self.dequeue_pointer(ring)?;
        self.memory
            .write32(self.context(INPUT_CONTEXT, index, 0), interval << 16);
        self.memory.write32(
            self.context(INPUT_CONTEXT, index, 1),
            EP_ERRORS | (kind << EP_TYPE_SHIFT) | (u32::from(max_packet) << EP_PACKET_SHIFT),
        );
        self.write_memory64(self.context(INPUT_CONTEXT, index, 2), pointer);
        self.memory
            .write32(self.context(INPUT_CONTEXT, index, 4), average);
        Ok(())
    }

    fn input_address(&self) -> Result<u64, Error> {
        self.address(INPUT_CONTEXT)
    }

    fn slot(&self, slot: u8) -> Result<Slot, Error> {
        self.slots
            .get(usize::from(slot))
            .copied()
            .flatten()
            .ok_or(Error::NoSlot)
    }

    // -----------------------------------------------------------------------
    // The pool
    // -----------------------------------------------------------------------

    /// A page of the pool, zeroed: its offset.
    fn take_page(&mut self) -> Result<usize, Error> {
        let index = (0..POOL_PAGES)
            .find(|index| {
                self.taken
                    .get(index / 64)
                    .is_some_and(|word| word & (1 << (index % 64)) == 0)
            })
            .ok_or(Error::Memory)?;
        if let Some(word) = self.taken.get_mut(index / 64) {
            *word |= 1 << (index % 64);
        }
        let page = self.pool + index * PAGE;
        for offset in (0..PAGE).step_by(4) {
            self.memory.write32(page + offset, 0);
        }
        Ok(page)
    }

    fn give_page(&mut self, page: usize) {
        let Some(index) = page.checked_sub(self.pool).map(|offset| offset / PAGE) else {
            return;
        };
        if let Some(word) = self.taken.get_mut(index / 64) {
            *word &= !(1 << (index % 64));
        }
    }

    /// A page of the pool made an empty ring.
    fn take_ring(&mut self) -> Result<Ring, Error> {
        let page = self.take_page()?;
        if let Err(error) = self.link(page) {
            self.give_page(page);
            return Err(error);
        }
        Ok(Ring {
            page,
            enqueue: 0,
            cycle: true,
        })
    }

    // -----------------------------------------------------------------------
    // Transfers
    // -----------------------------------------------------------------------

    /// Wait for the transfer on `slot`'s endpoint `dci` whose last TRB is at
    /// `last`: how it ended, and what a short data stage left.
    fn wait_transfer(
        &mut self,
        slot: u8,
        dci: u8,
        last: u64,
        limit: u64,
    ) -> (Option<(u8, u32)>, Option<u32>) {
        self.waiting = Some(Waiting {
            slot,
            dci,
            last,
            short: None,
            done: None,
        });
        self.ring_doorbell(slot, dci);
        let _ = self.wait_events(limit, |controller| {
            controller
                .waiting
                .is_some_and(|waiting| waiting.done.is_some())
        });
        let waiting = self.waiting.take();
        (
            waiting.and_then(|waiting| waiting.done),
            waiting.and_then(|waiting| waiting.short),
        )
    }

    /// What a transfer that ended with `code` failed with.
    const fn transfer_error(code: u8) -> Error {
        match code {
            CODE_STALL => Error::Stall,
            CODE_BABBLE => Error::Babble,
            CODE_DATA_BUFFER => Error::Buffer,
            CODE_TRANSACTION => Error::Transaction,
            code => Error::Command(code),
        }
    }

    /// `CLEAR_FEATURE(ENDPOINT_HALT)` to the endpoint at `address` of
    /// `target`: what a device is owed after its endpoint halted.
    fn clear_halt(&mut self, target: &Target, address: u8) {
        let _ = self.run_control(target, &Setup::clear_endpoint_halt(address), Data::None);
    }

    fn run_control(
        &mut self,
        target: &Target,
        setup: &Setup,
        data: Data<'_>,
    ) -> Result<usize, Error> {
        let length = data.len();
        if length > CONTROL_BYTES || length > usize::from(setup.length) {
            return Err(Error::TooLong);
        }
        if self.dead {
            return Err(Error::Unresponsive);
        }
        let slot_id = target.slot;
        let mut slot = self.slot(slot_id)?;
        if let Data::Out(bytes) = &data {
            for (at, &byte) in bytes.iter().enumerate() {
                self.memory.write8(CONTROL_BUFFER + at, byte);
            }
        }
        let inward = setup.is_in();
        let transfer_type = match (length, inward) {
            (0, _) => 0,
            (_, true) => TRT_IN,
            (_, false) => TRT_OUT,
        };
        let _ = self.enqueue(
            &mut slot.ring,
            u64::from_le_bytes(setup.bytes()),
            8,
            (TRB_SETUP << TRB_TYPE_SHIFT) | TRB_IMMEDIATE | transfer_type,
        );
        if length > 0 {
            let buffer = self.address(CONTROL_BUFFER)?;
            let direction = if inward { TRB_IN | TRB_SHORT } else { 0 };
            let _ = self.enqueue(
                &mut slot.ring,
                buffer,
                length as u32,
                (TRB_DATA << TRB_TYPE_SHIFT) | direction,
            );
        }
        // The status stage goes the other way from the data, and towards
        // the host when there was none.
        let status_direction = if inward && length > 0 { 0 } else { TRB_IN };
        let last = self.enqueue(
            &mut slot.ring,
            0,
            0,
            (TRB_STATUS << TRB_TYPE_SHIFT) | TRB_IOC | status_direction,
        );
        if let Some(entry) = self.slots.get_mut(usize::from(slot_id)) {
            *entry = Some(slot);
        }
        let (done, short) = self.wait_transfer(slot_id, DCI_CONTROL, last, TRANSFER_PATIENCE);
        match done {
            Some((CODE_SUCCESS, _)) => {
                let moved = length.saturating_sub(short.unwrap_or(0) as usize);
                if let Data::In(buffer) = data {
                    for (at, byte) in buffer.iter_mut().enumerate().take(moved) {
                        *byte = self.memory.read8(CONTROL_BUFFER + at);
                    }
                }
                Ok(moved)
            }
            Some((code, _)) => {
                self.restart_endpoint(slot_id, DCI_CONTROL, &slot.ring, true);
                Err(Self::transfer_error(code))
            }
            None => {
                if !self.dead {
                    let _ = self.endpoint_command(TRB_STOP_ENDPOINT, slot_id, DCI_CONTROL, 0);
                    self.restart_endpoint(slot_id, DCI_CONTROL, &slot.ring, false);
                }
                Err(Error::Timeout)
            }
        }
    }

    /// Recover every interrupt pipe an event marked halted: the endpoint
    /// reset on the controller and on the device, and its requests queued
    /// again, up to [`MAX_HALTS`] times in a row.
    fn recover_pipes(&mut self) {
        for index in 0..MAX_PIPES {
            let Some(Some(mut pipe)) = self.pipes.get(index).copied() else {
                continue;
            };
            if !pipe.halted {
                continue;
            }
            pipe.halted = false;
            pipe.halts += 1;
            if pipe.halts <= MAX_HALTS {
                let slot = pipe.target.slot;
                self.restart_endpoint(slot, pipe.dci, &pipe.ring, true);
                self.clear_halt(&pipe.target, pipe.number | 0x80);
                for _ in 0..QUEUED {
                    self.queue_request(&mut pipe);
                }
                self.ring_doorbell(slot, pipe.dci);
            }
            if let Some(entry) = self.pipes.get_mut(index) {
                *entry = Some(pipe);
            }
        }
    }

    /// An endpoint context's interval for an interrupt endpoint (Table
    /// 6-12): the exponent of a count of 125-microsecond frames.
    fn interval(speed: Speed, interval: u8) -> u32 {
        match speed {
            // Milliseconds, rounded down to a power of two of microframes.
            Speed::Low | Speed::Full => (u32::from(interval.max(1)) * 8).ilog2().clamp(3, 10),
            Speed::High | Speed::Super => u32::from(interval.clamp(1, 16)) - 1,
        }
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    fn address(&self, offset: usize) -> Result<u64, Error> {
        let address = if self.wide {
            self.memory.wide_address(offset)
        } else {
            self.memory.device_address(offset).map(u64::from)
        };
        address.ok_or(Error::Memory)
    }

    fn read(&self, register: u32) -> u32 {
        self.registers.read32(self.operational + register)
    }

    fn write(&mut self, register: u32, value: u32) {
        self.registers.write32(self.operational + register, value);
    }

    /// Write a 64-bit register: the low half first, as the specification
    /// asks of software that writes in halves (section 5.1).
    fn write64(&mut self, at: u32, value: u64) {
        self.registers.write32(at, value as u32);
        self.registers.write32(at + 4, (value >> 32) as u32);
    }

    fn write_memory64(&mut self, offset: usize, value: u64) {
        self.memory.write32(offset, value as u32);
        self.memory.write32(offset + 4, (value >> 32) as u32);
    }

    fn read_memory64(&self, offset: usize) -> u64 {
        u64::from(self.memory.read32(offset)) | (u64::from(self.memory.read32(offset + 4)) << 32)
    }

    /// Wait until `register & mask == value`.
    fn wait_register(&mut self, register: u32, mask: u32, value: u32, limit: u64) -> bool {
        let registers = &self.registers;
        let at = self.operational + register;
        crate::wait_for(&mut self.clock, limit, POLL_STEP, || {
            registers.read32(at) & mask == value
        })
    }
}

impl<R: Registers, D: Dma, C: Clock> Host for Controller<R, D, C> {
    type Clock = C;
    type Parts = Parts<R, D, C>;

    fn clock(&mut self) -> &mut C {
        &mut self.clock
    }

    fn shutdown(self) -> Result<Parts<R, D, C>, Parts<R, D, C>> {
        Controller::shutdown(self)
    }

    fn ports(&self) -> u32 {
        self.ports
    }

    fn root_port(&self, port: u32) -> RootPort {
        let value = self.port(port);
        RootPort {
            connected: value & PORT_CONNECTED != 0,
            changed: value & PORT_CONNECT_CHANGE != 0,
            enabled: value & PORT_ENABLED != 0,
        }
    }

    fn clear_root_changes(&mut self, port: u32) {
        let value = self.port(port);
        self.write_port(port, (value & PORT_KEEP) | (value & PORT_CHANGES));
    }

    /// A USB 3 port enables itself as its link trains, and its device is
    /// then in the default state already (section 4.3): it is reset only if
    /// it did not. A USB 2 port is always reset, which is what enables it.
    fn reset_root_port(&mut self, port: u32) -> RootReset {
        let value = self.port(port);
        if value & PORT_CONNECTED == 0 {
            return RootReset::Gone;
        }
        let usb3 = self.is_usb3(port);
        if !(usb3 && value & PORT_ENABLED != 0) {
            let reset = if usb3 { PORT_WARM_RESET } else { PORT_RESET };
            self.write_port(port, (value & PORT_KEEP) | reset);
            let done = PORT_RESET | PORT_RESET_CHANGE;
            let register = PORTSC + PORT_STRIDE * port;
            let _ = self.wait_register(register, done, PORT_RESET_CHANGE, PORT_RESET_PATIENCE);
        }
        Host::clear_root_changes(self, port);
        let value = self.port(port);
        if value & PORT_CONNECTED == 0 {
            RootReset::Gone
        } else if value & PORT_ENABLED == 0 {
            RootReset::Failed
        } else {
            self.clock.sleep_nanos(RESET_RECOVERY);
            RootReset::Ready(self.port_speed(port, value))
        }
    }

    fn ports_changed(&mut self) -> bool {
        core::mem::take(&mut self.ports_changed)
    }

    /// Enable Slot, the slot's contexts, and Address Device, which has the
    /// controller send `SET_ADDRESS` (sections 4.3.2 to 4.3.4).
    fn open(&mut self, place: &Place) -> Result<Target, Error> {
        let slot_type = self
            .slot_type
            .get(place.root_port as usize)
            .copied()
            .unwrap_or(0);
        let enabled = self.command(
            0,
            (TRB_ENABLE_SLOT << TRB_TYPE_SHIFT) | (u32::from(slot_type) << 16),
        )?;
        let id = enabled.slot;
        let mut target = Target {
            address: 0,
            speed: place.speed,
            max_packet: place.speed.default_max_packet(),
            translator: place.translator,
            slot: id,
            route: place.route,
            root_port: place.root_port,
        };
        let made = self.address_slot(&mut target);
        if made.is_err() {
            Host::release(self, &target);
        }
        made.map(|()| target)
    }

    /// Only the packet size: the controller gave the address in `open`.
    fn assign(&mut self, target: &mut Target, _address: u8, max_packet: u16) -> Result<(), Error> {
        if max_packet != target.max_packet {
            let slot = self.slot(target.slot)?;
            self.begin_input(ADD_CONTROL, 0, None);
            self.input_endpoint(DCI_CONTROL, EP_CONTROL, max_packet, 0, &slot.ring, 8)?;
            self.memory.barrier();
            let input = self.input_address()?;
            let _ = self.slot_command(TRB_EVALUATE_CONTEXT, target.slot, input)?;
            target.max_packet = max_packet;
        }
        Ok(())
    }

    /// A Configure Endpoint command whose only change is the slot context:
    /// the hub flag, its ports, and its think time (section 4.6.6; Linux
    /// does the same of a controller past revision 0.95).
    fn set_hub(&mut self, target: &Target, ports: u8, think_time: u8) -> Result<(), Error> {
        let slot = self.slot(target.slot)?;
        self.begin_input(ADD_SLOT, 0, Some((&slot, slot.entries)));
        let first = self.context(INPUT_CONTEXT, 1, 0);
        let value = self.memory.read32(first);
        self.memory.write32(first, value | SLOT_HUB);
        let second = self.context(INPUT_CONTEXT, 1, 1);
        let value = self.memory.read32(second) & 0x00FF_FFFF;
        self.memory
            .write32(second, value | (u32::from(ports) << 24));
        if target.speed == Speed::High {
            let third = self.context(INPUT_CONTEXT, 1, 2);
            let value = self.memory.read32(third) & !(0x3 << 16);
            self.memory
                .write32(third, value | (u32::from(think_time & 0x3) << 16));
        }
        self.memory.barrier();
        let input = self.input_address()?;
        self.slot_command(TRB_CONFIGURE_ENDPOINT, target.slot, input)
            .map(drop)
    }

    fn release(&mut self, target: &Target) {
        let id = target.slot;
        let Some(Some(slot)) = self.slots.get(usize::from(id)).copied() else {
            if id != 0 {
                let _ = self.slot_command(TRB_DISABLE_SLOT, id, 0);
            }
            return;
        };
        let disabled = self.slot_command(TRB_DISABLE_SLOT, id, 0).is_ok();
        self.write_memory64(DCBAA + usize::from(id) * 8, 0);
        self.memory.barrier();
        // What the controller may still be reading is kept, for good: a
        // page it was not told to forget is not one to hand out again.
        let mut pages = [None; 2 + 2 * MAX_PIPES + 3 * MAX_BULK + 2 * MAX_STREAMS];
        let mut count = 0;
        let mut note = |page: usize| {
            if let Some(entry) = pages.get_mut(count) {
                *entry = Some(page);
                count += 1;
            }
        };
        note(slot.context);
        note(slot.ring.page);
        for entry in &mut self.pipes {
            if let Some(pipe) = entry
                && pipe.target.slot == id
            {
                note(pipe.ring.page);
                note(pipe.buffers);
                *entry = None;
            }
        }
        for entry in &mut self.bulk {
            if let Some(pair) = entry
                && pair.target.slot == id
            {
                note(pair.ring_in.page);
                note(pair.ring_out.page);
                note(pair.buffer);
                *entry = None;
            }
        }
        for entry in &mut self.streams {
            if let Some(stream) = entry
                && stream.target.slot == id
            {
                note(stream.ring.page);
                note(stream.buffers);
                *entry = None;
            }
        }
        if disabled {
            for page in pages.into_iter().flatten() {
                self.give_page(page);
            }
        }
        if let Some(entry) = self.slots.get_mut(usize::from(id)) {
            *entry = None;
        }
    }

    fn control(&mut self, target: &Target, setup: &Setup, data: Data<'_>) -> Result<usize, Error> {
        self.run_control(target, setup, data)
    }

    fn open_pipe(&mut self, target: &Target, endpoint: &Endpoint) -> Result<usize, Error> {
        let length = usize::from(endpoint.max_packet);
        if length == 0 || length > PIPE_BYTES || endpoint.number == 0 || endpoint.number > 15 {
            return Err(Error::TooLong);
        }
        let index = self
            .pipes
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoPipe)?;
        let mut slot = self.slot(target.slot)?;
        let dci = endpoint.number * 2 + 1;
        let ring = self.take_ring()?;
        let buffers = match self.take_page() {
            Ok(page) => page,
            Err(error) => {
                self.give_page(ring.page);
                return Err(error);
            }
        };
        let entries = slot.entries.max(dci);
        self.begin_input(ADD_SLOT | (1 << dci), 0, Some((&slot, entries)));
        let configured = self
            .input_endpoint(
                dci,
                EP_INTERRUPT_IN,
                endpoint.max_packet,
                Self::interval(target.speed, endpoint.interval),
                &ring,
                // The average request, and above it the most one interval
                // carries: a packet.
                u32::from(endpoint.max_packet) | (u32::from(endpoint.max_packet) << 16),
            )
            .and_then(|()| {
                self.memory.barrier();
                let input = self.input_address()?;
                self.slot_command(TRB_CONFIGURE_ENDPOINT, target.slot, input)
            });
        if let Err(error) = configured {
            self.give_page(ring.page);
            self.give_page(buffers);
            return Err(error);
        }
        slot.entries = entries;
        if let Some(entry) = self.slots.get_mut(usize::from(target.slot)) {
            *entry = Some(slot);
        }
        let mut pipe = Pipe {
            target: *target,
            number: endpoint.number,
            dci,
            ring,
            buffers,
            length,
            halts: 0,
            halted: false,
        };
        for _ in 0..QUEUED {
            self.queue_request(&mut pipe);
        }
        if let Some(entry) = self.pipes.get_mut(index) {
            *entry = Some(pipe);
        }
        self.ring_doorbell(target.slot, dci);
        Ok(index)
    }

    /// Drop the endpoint from the slot's configuration; its pages are free
    /// again only if the controller took the command. A pipe of a device
    /// that is gone is closed the same way, and the slot's release, which
    /// follows, frees whatever this could not.
    fn close_pipe(&mut self, pipe: usize) {
        let Some(Some(state)) = self.pipes.get(pipe).copied() else {
            return;
        };
        let Ok(slot) = self.slot(state.target.slot) else {
            return;
        };
        self.begin_input(ADD_SLOT, 1 << state.dci, Some((&slot, slot.entries)));
        self.memory.barrier();
        let dropped = self
            .input_address()
            .and_then(|input| self.slot_command(TRB_CONFIGURE_ENDPOINT, state.target.slot, input));
        if dropped.is_ok() {
            self.give_page(state.ring.page);
            self.give_page(state.buffers);
            if let Some(entry) = self.pipes.get_mut(pipe) {
                *entry = None;
            }
        }
    }

    fn take_port_event(&mut self) -> Option<(u32, u32)> {
        let first = self.port_events.first_mut()?.take()?;
        self.port_events.rotate_left(1);
        Some(first)
    }

    fn root_port_raw(&self, port: u32) -> u32 {
        self.port(port)
    }

    fn take_trouble(&mut self) -> Option<Trouble> {
        let first = self.troubles.first_mut()?.take()?;
        self.troubles.rotate_left(1);
        Some(first)
    }

    fn pipe_stopped(&self, pipe: usize) -> bool {
        self.pipes
            .get(pipe)
            .copied()
            .flatten()
            .is_some_and(|state| state.halts > MAX_HALTS)
    }

    /// Acknowledge the interrupt, take every event, recover what failed,
    /// and hand on what arrived -- from this interrupt and from every wait
    /// since the last.
    fn service(&mut self, mut receive: impl FnMut(Packet)) -> Result<(), Error> {
        let status = self.read(USBSTS);
        // Only the bits this means to clear: the rest are write-one-to-clear
        // too, or reserved.
        self.write(USBSTS, status & (STS_EVENT | STS_PORT_CHANGE));
        let pending = self.registers.read32(self.runtime + IMAN);
        self.registers
            .write32(self.runtime + IMAN, pending | IMAN_PENDING);
        if status & (STS_SYSTEM_ERROR | STS_CONTROLLER_ERROR) != 0 {
            self.dead = true;
        }
        self.pump();
        self.recover_pipes();
        self.recover_streams();
        // Once more: an event that came while the busy flag was being
        // cleared raises no interrupt of its own.
        self.pump();
        while self.arrived_len > 0 {
            if let Some(packet) = self
                .arrived
                .get_mut(self.arrived_head)
                .and_then(Option::take)
            {
                receive(packet);
            }
            self.arrived_head = (self.arrived_head + 1) % ARRIVED;
            self.arrived_len -= 1;
        }
        if self.dead {
            Err(Error::Unresponsive)
        } else {
            Ok(())
        }
    }

    fn open_bulk(
        &mut self,
        target: &Target,
        inward: &Endpoint,
        outward: &Endpoint,
    ) -> Result<usize, Error> {
        self.open_pair(target, Some(inward), outward)
    }

    fn open_bulk_out(&mut self, target: &Target, outward: &Endpoint) -> Result<usize, Error> {
        self.open_pair(target, None, outward)
    }

    fn open_stream(&mut self, target: &Target, endpoint: &Endpoint) -> Result<usize, Error> {
        if usize::from(endpoint.max_packet) > STREAM_BYTES
            || endpoint.max_packet == 0
            || !(1..=15).contains(&endpoint.number)
        {
            return Err(Error::TooLong);
        }
        let index = self
            .streams
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoPipe)?;
        let mut slot = self.slot(target.slot)?;
        let dci = endpoint.number * 2 + 1;
        let ring = self.take_ring()?;
        let buffers = match self.take_page() {
            Ok(page) => page,
            Err(error) => {
                self.give_page(ring.page);
                return Err(error);
            }
        };
        let entries = slot.entries.max(dci);
        self.begin_input(ADD_SLOT | (1 << dci), 0, Some((&slot, entries)));
        let configured = self
            .input_endpoint(dci, EP_BULK_IN, endpoint.max_packet, 0, &ring, 3072)
            .and_then(|()| {
                self.memory.barrier();
                let input = self.input_address()?;
                self.slot_command(TRB_CONFIGURE_ENDPOINT, target.slot, input)
            });
        if let Err(error) = configured {
            self.give_page(ring.page);
            self.give_page(buffers);
            return Err(error);
        }
        slot.entries = entries;
        if let Some(entry) = self.slots.get_mut(usize::from(target.slot)) {
            *entry = Some(slot);
        }
        let mut stream = Stream {
            target: *target,
            number: endpoint.number,
            dci,
            ring,
            buffers,
            queued: 0,
            done: [None; STREAM_QUEUED],
            done_len: 0,
            halts: 0,
            halted: false,
        };
        for _ in 0..STREAM_QUEUED {
            self.queue_stream_request(&mut stream);
        }
        if let Some(entry) = self.streams.get_mut(index) {
            *entry = Some(stream);
        }
        self.ring_doorbell(target.slot, dci);
        Ok(index)
    }

    fn take_chunk(&mut self, stream: usize) -> Option<Chunk> {
        let mut state = self.streams.get(stream).copied().flatten()?;
        if state.done_len == 0 {
            return None;
        }
        let (buffer, len) = state.done.first().copied().flatten()?;
        state.done.rotate_left(1);
        if let Some(last) = state.done.last_mut() {
            *last = None;
        }
        state.done_len -= 1;
        let len = len.min(STREAM_BYTES);
        let mut chunk = Chunk {
            bytes: [0; STREAM_BYTES],
            len,
        };
        for (at, byte) in chunk.bytes.iter_mut().enumerate().take(len) {
            *byte = self.memory.read8(buffer + at);
        }
        // The request it was is queued again, unless the stream is halted
        // or given up: its recovery queues its own.
        if !state.halted && state.halts <= MAX_HALTS && !self.dead {
            self.queue_stream_request(&mut state);
            self.ring_doorbell(state.target.slot, state.dci);
        }
        if let Some(entry) = self.streams.get_mut(stream) {
            *entry = Some(state);
        }
        Some(chunk)
    }

    fn close_stream(&mut self, stream: usize) {
        let Some(Some(state)) = self.streams.get(stream).copied() else {
            return;
        };
        let Ok(slot) = self.slot(state.target.slot) else {
            return;
        };
        self.begin_input(ADD_SLOT, 1 << state.dci, Some((&slot, slot.entries)));
        self.memory.barrier();
        let dropped = self
            .input_address()
            .and_then(|input| self.slot_command(TRB_CONFIGURE_ENDPOINT, state.target.slot, input));
        if dropped.is_ok() {
            self.give_page(state.ring.page);
            self.give_page(state.buffers);
            if let Some(entry) = self.streams.get_mut(stream) {
                *entry = None;
            }
        }
    }

    fn bulk(&mut self, pair: usize, data: Data<'_>) -> Result<usize, Error> {
        let length = data.len();
        if length > BULK_BYTES {
            return Err(Error::TooLong);
        }
        if self.dead {
            return Err(Error::Unresponsive);
        }
        let Some(Some(mut state)) = self.bulk.get(pair).copied() else {
            return Err(Error::NoPipe);
        };
        let inward = match &data {
            Data::None => return Ok(0),
            // A pair opened with no IN endpoint: its IN is a stream's.
            Data::In(_) if state.number_in == 0 => return Err(Error::NoPipe),
            Data::In(_) => true,
            Data::Out(bytes) => {
                for (at, &byte) in bytes.iter().enumerate() {
                    self.memory.write8(state.buffer + at, byte);
                }
                false
            }
        };
        let (dci, address, mut ring) = if inward {
            (
                state.number_in * 2 + 1,
                state.number_in | 0x80,
                state.ring_in,
            )
        } else {
            (state.number_out * 2, state.number_out, state.ring_out)
        };
        let buffer = self.address(state.buffer)?;
        let last = self.enqueue(
            &mut ring,
            buffer,
            length as u32,
            (TRB_NORMAL << TRB_TYPE_SHIFT) | TRB_IOC | TRB_SHORT,
        );
        if inward {
            state.ring_in = ring;
        } else {
            state.ring_out = ring;
        }
        if let Some(entry) = self.bulk.get_mut(pair) {
            *entry = Some(state);
        }
        let slot = state.target.slot;
        let (done, _) = self.wait_transfer(slot, dci, last, BULK_PATIENCE);
        match done {
            Some((CODE_SUCCESS | CODE_SHORT_PACKET, left)) => {
                let moved = length.saturating_sub(left as usize);
                if let Data::In(into) = data {
                    for (at, byte) in into.iter_mut().enumerate().take(moved) {
                        *byte = self.memory.read8(state.buffer + at);
                    }
                }
                Ok(moved)
            }
            Some((code, _)) => {
                self.restart_endpoint(slot, dci, &ring, true);
                if code == CODE_STALL {
                    self.clear_halt(&state.target, address);
                }
                Err(Self::transfer_error(code))
            }
            None => {
                if !self.dead {
                    let _ = self.endpoint_command(TRB_STOP_ENDPOINT, slot, dci, 0);
                    self.restart_endpoint(slot, dci, &ring, false);
                }
                Err(Error::Timeout)
            }
        }
    }

    fn close_bulk(&mut self, pair: usize) {
        let Some(Some(state)) = self.bulk.get(pair).copied() else {
            return;
        };
        let Ok(slot) = self.slot(state.target.slot) else {
            return;
        };
        let drop_in = if state.number_in == 0 {
            0
        } else {
            1 << (state.number_in * 2 + 1)
        };
        let drop = drop_in | (1 << (state.number_out * 2));
        self.begin_input(ADD_SLOT, drop, Some((&slot, slot.entries)));
        self.memory.barrier();
        let dropped = self
            .input_address()
            .and_then(|input| self.slot_command(TRB_CONFIGURE_ENDPOINT, state.target.slot, input));
        if dropped.is_ok() {
            self.give_page(state.ring_in.page);
            self.give_page(state.ring_out.page);
            self.give_page(state.buffer);
            if let Some(entry) = self.bulk.get_mut(pair) {
                *entry = None;
            }
        }
    }
}

impl<R: Registers, D: Dma, C: Clock> Controller<R, D, C> {
    /// [`Host::open_bulk`] and [`Host::open_bulk_out`]: the OUT endpoint,
    /// and the IN one if there is one. The pair takes its three pages
    /// either way, so closing or releasing it gives back the same three.
    fn open_pair(
        &mut self,
        target: &Target,
        inward: Option<&Endpoint>,
        outward: &Endpoint,
    ) -> Result<usize, Error> {
        let valid = |endpoint: &Endpoint| (1..=15).contains(&endpoint.number);
        if !inward.is_none_or(valid) || !valid(outward) {
            return Err(Error::TooLong);
        }
        let index = self
            .bulk
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoPipe)?;
        let mut slot = self.slot(target.slot)?;
        let dci_in = inward.map(|inward| inward.number * 2 + 1);
        let dci_out = outward.number * 2;
        let mut pages = [0_usize; 3];
        for at in 0..3 {
            match self.take_page() {
                Ok(page) => {
                    if let Some(entry) = pages.get_mut(at) {
                        *entry = page;
                    }
                }
                Err(error) => {
                    for &page in pages.iter().take(at) {
                        self.give_page(page);
                    }
                    return Err(error);
                }
            }
        }
        let [in_page, out_page, buffer] = pages;
        let ring = |page| Ring {
            page,
            enqueue: 0,
            cycle: true,
        };
        let (ring_in, ring_out) = (ring(in_page), ring(out_page));
        let entries = slot.entries.max(dci_in.unwrap_or(0)).max(dci_out);
        let add_in = dci_in.map_or(0, |dci| 1 << dci);
        self.begin_input(
            ADD_SLOT | add_in | (1 << dci_out),
            0,
            Some((&slot, entries)),
        );
        // Three kilobytes: the average the specification suggests for a
        // bulk endpoint (section 4.14.1.1).
        let configured = self
            .link(in_page)
            .and_then(|()| self.link(out_page))
            .and_then(|()| match (dci_in, inward) {
                (Some(dci), Some(inward)) => {
                    self.input_endpoint(dci, EP_BULK_IN, inward.max_packet, 0, &ring_in, 3072)
                }
                _ => Ok(()),
            })
            .and_then(|()| {
                self.input_endpoint(dci_out, EP_BULK_OUT, outward.max_packet, 0, &ring_out, 3072)
            })
            .and_then(|()| {
                self.memory.barrier();
                let input = self.input_address()?;
                self.slot_command(TRB_CONFIGURE_ENDPOINT, target.slot, input)
            });
        if let Err(error) = configured {
            for page in pages {
                self.give_page(page);
            }
            return Err(error);
        }
        slot.entries = entries;
        if let Some(entry) = self.slots.get_mut(usize::from(target.slot)) {
            *entry = Some(slot);
        }
        if let Some(entry) = self.bulk.get_mut(index) {
            *entry = Some(Bulk {
                target: *target,
                number_in: inward.map_or(0, |inward| inward.number),
                number_out: outward.number,
                ring_in,
                ring_out,
                buffer,
            });
        }
        Ok(index)
    }

    /// [`Host::open`], once the slot is enabled: its contexts, its ring, and
    /// the Address Device command.
    fn address_slot(&mut self, target: &mut Target) -> Result<(), Error> {
        let id = target.slot;
        if id == 0 || usize::from(id) > MAX_SLOTS {
            return Err(Error::NoSlot);
        }
        let context = self.take_page()?;
        let ring = match self.take_ring() {
            Ok(ring) => ring,
            Err(error) => {
                self.give_page(context);
                return Err(error);
            }
        };
        let slot = Slot {
            context,
            ring,
            entries: DCI_CONTROL,
        };
        if let Some(entry) = self.slots.get_mut(usize::from(id)) {
            *entry = Some(slot);
        }
        let output = self.address(context)?;
        self.write_memory64(DCBAA + usize::from(id) * 8, output);

        self.begin_input(ADD_SLOT | ADD_CONTROL, 0, None);
        // Table 6-4: the route, the speed -- which revision 1.2 no longer
        // reads and older controllers do -- and one context entry.
        let speed = match target.speed {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
        };
        self.memory.write32(
            self.context(INPUT_CONTEXT, 1, 0),
            (target.route & 0xF_FFFF)
                | (speed << SLOT_SPEED_SHIFT)
                | (u32::from(DCI_CONTROL) << SLOT_ENTRIES_SHIFT),
        );
        // Table 6-5: root hub ports count from one.
        self.memory.write32(
            self.context(INPUT_CONTEXT, 1, 1),
            ((target.root_port + 1) & 0xFF) << 16,
        );
        // Table 6-6: the hub whose transaction translator a full- or
        // low-speed device is reached through, by its slot, and the port.
        if let Some((hub, port)) = target.translator
            && matches!(target.speed, Speed::Low | Speed::Full)
        {
            self.memory.write32(
                self.context(INPUT_CONTEXT, 1, 2),
                u32::from(hub) | (u32::from(port) << 8),
            );
        }
        self.input_endpoint(DCI_CONTROL, EP_CONTROL, target.max_packet, 0, &ring, 8)?;
        self.memory.barrier();
        let input = self.input_address()?;
        let _ = self.slot_command(TRB_ADDRESS_DEVICE, id, input)?;
        // Table 6-7: the address the controller chose.
        target.address = (self.memory.read32(self.context(context, 0, 3)) & 0xFF) as u8;
        self.clock.sleep_nanos(ADDRESS_RECOVERY);
        Ok(())
    }
}
