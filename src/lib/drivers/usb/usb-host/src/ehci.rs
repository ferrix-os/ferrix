//! An EHCI controller, as the Enhanced Host Controller Interface
//! specification 1.0 describes it: its reset, its two schedules, control
//! transfers on the asynchronous one, and interrupt IN pipes on the periodic
//! one.
//!
//! # The memory
//!
//! Four pages, [`AREA_BYTES`], that the program and the controller see
//! alike. Nothing in them crosses a page, so the page-at-a-time addresses a
//! pin gives are all the controller needs:
//!
//! * page 0: the periodic frame list, 1024 entries, every one pointing at
//!   one inactive *anchor* queue head, after which the interrupt pipes'
//!   queue heads are linked -- so a pipe joins or leaves every frame with one
//!   pointer, and the frame list is written once;
//! * page 1: the control queue head, the anchor, [`MAX_PIPES`] pipe queue
//!   heads, and the transfer descriptors (qTDs) of all of them;
//! * page 2: the SETUP packet and the control data buffer;
//! * page 3: each pipe's two buffers.
//!
//! # Control transfers
//!
//! The asynchronous schedule holds one queue head, the control one, and runs
//! only for the length of a transfer: set up with the schedule off, started,
//! waited for, stopped. A queue head the controller is not walking is one
//! software may rewrite, so the one head serves every device and speed, and
//! no doorbell handshake is needed to take a head out. A transfer costs a
//! millisecond or two more than it would with the schedule left running, and
//! transfers happen only while a device is set up and a hub's ports polled.
//!
//! # Interrupt pipes
//!
//! Each pipe is a queue head with two qTDs that point at each other. The
//! controller finishes one, finds the other active and carries on with it,
//! while the program reads the finished one and arms it again; a qTD found
//! inactive stops the queue head where it is until it is armed. Every pipe
//! is polled every frame, which is as often as any interval allows. A full-
//! or low-speed device behind a high-speed hub is reached through the hub's
//! transaction translator: a start-split in microframe 0 and complete-splits
//! in microframes 2 to 4, the budget EHCI's section 4.12.2 gives a packet of
//! up to 64 bytes.

pub use crate::host::{
    CONTROL_BYTES, Data, Error, PIPE_BYTES, Packet, RootPort, RootReset, Target,
};
use crate::host::{Endpoint, Host, Place};
use crate::usb::{Setup, Speed};
use crate::{Clock, Dma, MILLISECOND, Parts, Registers, wait_for};

// ---------------------------------------------------------------------------
// Registers
// ---------------------------------------------------------------------------

/// `CAPLENGTH` (and `HCIVERSION` above it).
const CAPLENGTH: u32 = 0x00;
/// `HCSPARAMS`.
const HCSPARAMS: u32 = 0x04;
/// `HCCPARAMS`.
const HCCPARAMS: u32 = 0x08;

// The operational registers, from `CAPLENGTH`.
const USBCMD: u32 = 0x00;
const USBSTS: u32 = 0x04;
const USBINTR: u32 = 0x08;
const CTRLDSSEGMENT: u32 = 0x10;
const PERIODICLISTBASE: u32 = 0x14;
const ASYNCLISTADDR: u32 = 0x18;
const CONFIGFLAG: u32 = 0x40;
const PORTSC: u32 = 0x44;

/// `HCSPARAMS`: how many root ports.
const PORTS_MASK: u32 = 0xF;
/// `HCSPARAMS`: port power control, which software switches.
const PORT_POWER_CONTROL: u32 = 1 << 4;
/// `HCCPARAMS`: 64-bit addressing, which only moves the high words to
/// `CTRLDSSEGMENT`, left zero.
const ADDRESSING_64: u32 = 1 << 0;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_PERIODIC: u32 = 1 << 4;
const CMD_ASYNC: u32 = 1 << 5;
/// `USBCMD`'s interrupt threshold: at most one interrupt per frame.
const CMD_THRESHOLD_FRAME: u32 = 0x08 << 16;

/// `USBSTS`: a transfer with IOC set finished, or one was short.
const STS_INTERRUPT: u32 = 1 << 0;
/// `USBSTS`: a transfer ended in an error.
const STS_ERROR: u32 = 1 << 1;
/// `USBSTS`: the controller hit an error on the system bus and halted.
const STS_SYSTEM_ERROR: u32 = 1 << 4;
/// `USBSTS`: every write-one-to-clear bit.
const STS_ACKNOWLEDGE: u32 = 0x3F;
const STS_HALTED: u32 = 1 << 12;
const STS_PERIODIC: u32 = 1 << 14;
const STS_ASYNC: u32 = 1 << 15;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_CONNECT_CHANGE: u32 = 1 << 1;
const PORT_ENABLED: u32 = 1 << 2;
const PORT_ENABLE_CHANGE: u32 = 1 << 3;
const PORT_OVER_CURRENT_CHANGE: u32 = 1 << 5;
const PORT_RESET: u32 = 1 << 8;
const PORT_LINE_STATUS: u32 = 0b11 << 10;
/// `PORTSC`'s line status for a low-speed device: K-state.
const PORT_LINE_K: u32 = 0b01 << 10;
const PORT_POWER: u32 = 1 << 12;
const PORT_OWNER: u32 = 1 << 13;
/// `PORTSC`'s write-one-to-clear bits, which a write that changes something
/// else must leave zero.
const PORT_CHANGES: u32 = PORT_CONNECT_CHANGE | PORT_ENABLE_CHANGE | PORT_OVER_CURRENT_CHANGE;

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

/// A link pointer's terminate bit.
const TERMINATE: u32 = 1;
/// A link pointer's type field for a queue head.
const TYPE_QH: u32 = 0b01 << 1;

// A queue head's words, by byte offset.
const QH_LINK: usize = 0;
const QH_CHARACTERISTICS: usize = 4;
const QH_CAPABILITIES: usize = 8;
const QH_CURRENT: usize = 12;
const QH_NEXT: usize = 16;
const QH_ALTERNATE: usize = 20;
const QH_TOKEN: usize = 24;
const QH_BUFFERS: usize = 28;
/// A queue head's size, with the five buffer pointers.
const QH_WORDS: usize = 12;

// A qTD's words.
const QTD_NEXT: usize = 0;
const QTD_ALTERNATE: usize = 4;
const QTD_TOKEN: usize = 8;
const QTD_BUFFER: usize = 12;
const QTD_WORDS: usize = 8;

// Endpoint characteristics.
const EPS_FULL: u32 = 0;
const EPS_LOW: u32 = 1;
const EPS_HIGH: u32 = 2;
const DATA_TOGGLE_FROM_QTD: u32 = 1 << 14;
const HEAD_OF_LIST: u32 = 1 << 15;
const CONTROL_ENDPOINT: u32 = 1 << 27;
/// Endpoint capabilities: one transaction per microframe.
const ONE_PER_MICROFRAME: u32 = 1 << 30;
/// The start-split and complete-split masks of a split interrupt pipe.
const SPLIT_START: u32 = 0x01;
const SPLIT_COMPLETE: u32 = 0x1C;
/// The start mask of a high-speed interrupt pipe: microframe 0.
const HIGH_SPEED_START: u32 = 0x01;
/// The NAK counter reload for a high-speed control endpoint, as Linux uses.
const NAK_RELOAD: u32 = 4;

// qTD token.
const TOKEN_ACTIVE: u32 = 1 << 7;
const TOKEN_HALTED: u32 = 1 << 6;
const TOKEN_BUFFER_ERROR: u32 = 1 << 5;
const TOKEN_BABBLE: u32 = 1 << 4;
const TOKEN_TRANSACTION: u32 = 1 << 3;
const PID_OUT: u32 = 0 << 8;
const PID_IN: u32 = 1 << 8;
const PID_SETUP: u32 = 2 << 8;
/// Three tries before a transaction error halts the queue.
const TOKEN_TRIES: u32 = 3 << 10;
const TOKEN_IOC: u32 = 1 << 15;
const TOKEN_TOGGLE: u32 = 1 << 31;
const TOKEN_BYTES_SHIFT: u32 = 16;
const TOKEN_BYTES_MASK: u32 = 0x7FFF;

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// A page.
const PAGE: usize = 4096;
/// The memory the controller is given.
pub const AREA_BYTES: usize = 4 * PAGE;
/// The frame list's entries.
const FRAMES: usize = 1024;
/// The frame list.
const FRAME_LIST: usize = 0;
/// Queue heads are 48 bytes, aligned to 32; each gets 64.
const QH_SLOT: usize = 64;
const CONTROL_QH: usize = PAGE;
const ANCHOR_QH: usize = PAGE + QH_SLOT;
/// How many interrupt pipes there may be at once.
pub const MAX_PIPES: usize = 16;
const FIRST_PIPE_QH: usize = PAGE + 2 * QH_SLOT;
/// qTDs are 32 bytes and aligned to 32.
const QTD_SLOT: usize = 32;
const CONTROL_QTDS: usize = FIRST_PIPE_QH + MAX_PIPES * QH_SLOT;
const FIRST_PIPE_QTD: usize = CONTROL_QTDS + 3 * QTD_SLOT;
const SETUP_BUFFER: usize = 2 * PAGE;
const CONTROL_BUFFER: usize = SETUP_BUFFER + 64;
const FIRST_PIPE_BUFFER: usize = 3 * PAGE;

const _: () = assert!(
    FIRST_PIPE_QTD + MAX_PIPES * 2 * QTD_SLOT <= 2 * PAGE,
    "descriptors fit page 1"
);
const _: () = assert!(
    CONTROL_BUFFER + CONTROL_BYTES <= 3 * PAGE,
    "control buffers fit page 2"
);
const _: () = assert!(
    FIRST_PIPE_BUFFER + MAX_PIPES * 2 * PIPE_BYTES <= AREA_BYTES,
    "pipe buffers fit page 3"
);

const fn pipe_qh(pipe: usize) -> usize {
    FIRST_PIPE_QH + pipe * QH_SLOT
}

const fn pipe_qtd(pipe: usize, which: usize) -> usize {
    FIRST_PIPE_QTD + (pipe * 2 + which) * QTD_SLOT
}

const fn pipe_buffer(pipe: usize, which: usize) -> usize {
    FIRST_PIPE_BUFFER + (pipe * 2 + which) * PIPE_BYTES
}

// ---------------------------------------------------------------------------
// Waits
// ---------------------------------------------------------------------------

/// How long a halt, a schedule switch or a port reset's end may take: the
/// specification gives each 16 microframes or 2 ms; this allows ten times.
const REGISTER_PATIENCE: u64 = 20 * MILLISECOND;
/// How long `HCRESET` may take.
const RESET_PATIENCE: u64 = 250 * MILLISECOND;
/// How long a control transfer may take before it is abandoned.
const TRANSFER_PATIENCE: u64 = 1000 * MILLISECOND;
/// How often a wait looks.
const POLL_STEP: u64 = MILLISECOND / 4;
/// How long a root port is held in reset: USB 2.0's `TDRSTR`.
const ROOT_RESET: u64 = 50 * MILLISECOND;
/// How long a port's power takes to settle when the controller switches it.
const POWER_SETTLE: u64 = 20 * MILLISECOND;

// ---------------------------------------------------------------------------
// The controller
// ---------------------------------------------------------------------------

/// One interrupt pipe's bookkeeping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Pipe {
    /// Which of the two qTDs the controller finishes next.
    next: usize,
    /// How many bytes each qTD asks for.
    length: usize,
    /// Halts in a row, reset by a good completion.
    halts: u32,
}

/// Halts in a row after which a pipe is left stopped: the device is most
/// likely gone, and the next hub poll will say so.
const MAX_HALTS: u32 = 4;

/// The controller, running, and its memory.
#[derive(Debug)]
pub struct Controller<R, D, C> {
    registers: R,
    memory: D,
    clock: C,
    /// Where the operational registers start.
    operational: u32,
    ports: u32,
    pipes: [Option<Pipe>; MAX_PIPES],
}

impl<R: Registers, D: Dma, C: Clock> Controller<R, D, C> {
    /// Halt and reset the controller, lay its memory out, start it with the
    /// periodic schedule running and every root port powered.
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
        let parameters = registers.read32(HCSPARAMS);
        let mut controller = Controller {
            registers,
            memory,
            clock,
            operational,
            ports: parameters & PORTS_MASK,
            pipes: [None; MAX_PIPES],
        };
        match controller.bring_up(parameters & PORT_POWER_CONTROL != 0) {
            Ok(()) => Ok(controller),
            Err(error) => Err((error, controller.into_parts())),
        }
    }

    fn bring_up(&mut self, switched_power: bool) -> Result<(), Error> {
        let frame_list = self.address(FRAME_LIST)?;
        let control = self.address(CONTROL_QH)?;
        let anchor = self.address(ANCHOR_QH)?;
        // Every page where the controller can reach it, and page-aligned
        // there: the frame list must be, and a queue head or qTD is aligned
        // only as far as its page is.
        let aligned = |page: usize| {
            self.memory
                .device_address(page)
                .is_some_and(|address| (address as usize).is_multiple_of(PAGE))
        };
        if self.memory.len() < AREA_BYTES || !(0..AREA_BYTES).step_by(PAGE).all(aligned) {
            return Err(Error::Memory);
        }
        self.halt()?;
        self.write(USBCMD, CMD_RESET);
        if !self.wait_register(USBCMD, CMD_RESET, 0, RESET_PATIENCE) {
            return Err(Error::Unresponsive);
        }

        for offset in (0..AREA_BYTES).step_by(4) {
            self.memory.write32(offset, 0);
        }
        // The anchor: never executed (no start mask), links to nothing yet.
        self.memory.write32(ANCHOR_QH + QH_LINK, TERMINATE);
        self.memory.write32(ANCHOR_QH + QH_NEXT, TERMINATE);
        self.memory.write32(ANCHOR_QH + QH_ALTERNATE, TERMINATE);
        for frame in 0..FRAMES {
            self.memory
                .write32(FRAME_LIST + frame * 4, anchor | TYPE_QH);
        }
        self.memory.write32(CONTROL_QH + QH_LINK, control | TYPE_QH);
        self.memory.barrier();

        if self.registers.read32(HCCPARAMS) & ADDRESSING_64 != 0 {
            self.write(CTRLDSSEGMENT, 0);
        }
        self.write(PERIODICLISTBASE, frame_list);
        self.write(ASYNCLISTADDR, control);
        self.write(USBSTS, STS_ACKNOWLEDGE);
        self.write(USBINTR, STS_INTERRUPT | STS_ERROR | STS_SYSTEM_ERROR);
        self.write(USBCMD, CMD_THRESHOLD_FRAME | CMD_PERIODIC | CMD_RUN);
        // Every port routed to this controller rather than its companion.
        self.write(CONFIGFLAG, 1);
        if !self.wait_register(USBSTS, STS_HALTED, 0, REGISTER_PATIENCE) {
            return Err(Error::Unresponsive);
        }
        if !self.wait_register(USBSTS, STS_PERIODIC, STS_PERIODIC, REGISTER_PATIENCE) {
            return Err(Error::Schedule);
        }
        if switched_power {
            for port in 0..self.ports {
                let value = self.port_register(port) & !PORT_CHANGES;
                self.write(PORTSC + 4 * port, value | PORT_POWER);
            }
            self.clock.sleep_nanos(POWER_SETTLE);
        }
        Ok(())
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
        self.write(USBCMD, command & !(CMD_RUN | CMD_PERIODIC | CMD_ASYNC));
        if self.wait_register(USBSTS, STS_HALTED, STS_HALTED, REGISTER_PATIENCE) {
            Ok(())
        } else {
            Err(Error::Unresponsive)
        }
    }

    /// The clock the controller waits on.
    pub fn clock(&mut self) -> &mut C {
        &mut self.clock
    }

    /// How many root ports there are.
    #[must_use]
    pub const fn ports(&self) -> u32 {
        self.ports
    }

    /// Root port `port`, counting from zero.
    #[must_use]
    pub fn root_port(&self, port: u32) -> RootPort {
        let value = self.port_register(port);
        RootPort {
            connected: value & PORT_CONNECTED != 0,
            changed: value & PORT_CONNECT_CHANGE != 0,
            enabled: value & PORT_ENABLED != 0,
        }
    }

    /// Clear a root port's change bits.
    pub fn clear_root_changes(&mut self, port: u32) {
        let value = self.port_register(port);
        self.write(PORTSC + 4 * port, value);
    }

    /// Reset root port `port` and say what is there.
    ///
    /// A low-speed device shows as a K state before the reset, and a
    /// full-speed one as a port the reset did not enable; both go to the
    /// companion controller, as the specification's section 4.2.2 says.
    pub fn reset_root_port(&mut self, port: u32) -> RootReset {
        let register = PORTSC + 4 * port;
        let value = self.port_register(port) & !PORT_CHANGES;
        if value & PORT_CONNECTED == 0 {
            return RootReset::Gone;
        }
        if value & PORT_LINE_STATUS == PORT_LINE_K {
            self.write(register, value | PORT_OWNER);
            return RootReset::Handed;
        }
        self.write(register, (value | PORT_RESET) & !PORT_ENABLED);
        self.clock.sleep_nanos(ROOT_RESET);
        let value = self.port_register(port) & !PORT_CHANGES;
        self.write(register, value & !PORT_RESET);
        let _ = self.wait_register(register, PORT_RESET, 0, REGISTER_PATIENCE);
        let value = self.port_register(port);
        if value & PORT_CONNECTED == 0 {
            RootReset::Gone
        } else if value & PORT_ENABLED != 0 {
            RootReset::Ready(Speed::High)
        } else {
            self.write(register, (value & !PORT_CHANGES) | PORT_OWNER);
            RootReset::Handed
        }
    }

    fn port_register(&self, port: u32) -> u32 {
        self.read(PORTSC + 4 * port)
    }

    // -----------------------------------------------------------------------
    // Control transfers
    // -----------------------------------------------------------------------

    /// Run one control transfer to completion: how many bytes the data stage
    /// moved.
    ///
    /// # Errors
    ///
    /// What the transfer ended in, or [`Error::TooLong`] for a data stage
    /// past [`CONTROL_BYTES`].
    pub fn control(
        &mut self,
        target: &Target,
        setup: &Setup,
        data: Data<'_>,
    ) -> Result<usize, Error> {
        let length = data.len();
        if length > CONTROL_BYTES || length > usize::from(setup.length) {
            return Err(Error::TooLong);
        }
        if let Data::Out(bytes) = &data {
            for (at, &byte) in bytes.iter().enumerate() {
                self.memory.write8(CONTROL_BUFFER + at, byte);
            }
        }
        for (at, byte) in setup.bytes().into_iter().enumerate() {
            self.memory.write8(SETUP_BUFFER + at, byte);
        }
        let inward = setup.is_in();
        self.build_control(target, length, inward)?;
        let outcome = self.run_async();
        let moved = self.control_outcome(outcome, length)?;
        if let Data::In(buffer) = data {
            for (at, slot) in buffer.iter_mut().enumerate().take(moved) {
                *slot = self.memory.read8(CONTROL_BUFFER + at);
            }
        }
        Ok(moved)
    }

    /// Write the control queue head and its three qTDs: SETUP, the data
    /// stage if there is one, and the status stage the other way.
    fn build_control(&mut self, target: &Target, length: usize, inward: bool) -> Result<(), Error> {
        let setup_qtd = CONTROL_QTDS;
        let data_qtd = CONTROL_QTDS + QTD_SLOT;
        let status_qtd = CONTROL_QTDS + 2 * QTD_SLOT;
        let setup_buffer = self.address(SETUP_BUFFER)?;
        let control_buffer = self.address(CONTROL_BUFFER)?;
        let bytes = u32::try_from(length).map_err(|_| Error::TooLong)?;

        let after_setup = if length > 0 { data_qtd } else { status_qtd };
        self.write_qtd(
            setup_qtd,
            Some(after_setup),
            token(PID_SETUP, 8, false, false),
            setup_buffer,
        )?;
        if length > 0 {
            let pid = if inward { PID_IN } else { PID_OUT };
            self.write_qtd(
                data_qtd,
                Some(status_qtd),
                token(pid, bytes, true, false),
                control_buffer,
            )?;
        }
        // The status stage goes the other way from the data, and IN when
        // there was none.
        let status_pid = if inward && length > 0 {
            PID_OUT
        } else {
            PID_IN
        };
        self.write_qtd(status_qtd, None, token(status_pid, 0, true, true), 0)?;

        let (characteristics, capabilities) = endpoint(target, 0, target.max_packet, true);
        let control = self.address(CONTROL_QH)?;
        let first = self.address(setup_qtd)?;
        self.memory.write32(CONTROL_QH + QH_LINK, control | TYPE_QH);
        self.memory
            .write32(CONTROL_QH + QH_CHARACTERISTICS, characteristics);
        self.memory
            .write32(CONTROL_QH + QH_CAPABILITIES, capabilities);
        self.memory.write32(CONTROL_QH + QH_CURRENT, 0);
        self.memory.write32(CONTROL_QH + QH_NEXT, first);
        self.memory.write32(CONTROL_QH + QH_ALTERNATE, TERMINATE);
        self.memory.write32(CONTROL_QH + QH_TOKEN, 0);
        for word in 0..5 {
            self.memory.write32(CONTROL_QH + QH_BUFFERS + 4 * word, 0);
        }
        self.memory.barrier();
        Ok(())
    }

    /// Run the asynchronous schedule until the control transfer's last qTD
    /// is done or one halts: whether it finished in time.
    fn run_async(&mut self) -> Result<(), Error> {
        let command = self.read(USBCMD);
        self.write(USBCMD, command | CMD_ASYNC);
        if !self.wait_register(USBSTS, STS_ASYNC, STS_ASYNC, REGISTER_PATIENCE) {
            return Err(Error::Schedule);
        }
        let status_qtd = CONTROL_QTDS + 2 * QTD_SLOT;
        let finished = {
            let memory = &self.memory;
            wait_for(&mut self.clock, TRANSFER_PATIENCE, POLL_STEP, || {
                let token = memory.read32(CONTROL_QH + QH_TOKEN);
                let last = memory.read32(status_qtd + QTD_TOKEN);
                token & TOKEN_HALTED != 0 || last & TOKEN_ACTIVE == 0
            })
        };
        let command = self.read(USBCMD);
        self.write(USBCMD, command & !CMD_ASYNC);
        let stopped = self.wait_register(USBSTS, STS_ASYNC, 0, REGISTER_PATIENCE);
        if !finished {
            return Err(Error::Timeout);
        }
        if !stopped {
            return Err(Error::Schedule);
        }
        Ok(())
    }

    /// What the control transfer's qTDs say: the data stage's bytes moved,
    /// or the error the first unfinished one ended in.
    fn control_outcome(&self, ran: Result<(), Error>, length: usize) -> Result<usize, Error> {
        let data_qtd = CONTROL_QTDS + QTD_SLOT;
        let used = if length > 0 { 3 } else { 2 };
        for index in 0..3 {
            if index == 1 && used == 2 {
                continue;
            }
            let qtd_token = self
                .memory
                .read32(CONTROL_QTDS + index * QTD_SLOT + QTD_TOKEN);
            if let Some(error) = token_error(qtd_token) {
                return Err(error);
            }
            if qtd_token & TOKEN_ACTIVE != 0 {
                ran?;
                // Finished with this qTD still active: the transfer halted
                // on the queue head's overlay before reaching it.
                let overlay = self.memory.read32(CONTROL_QH + QH_TOKEN);
                return Err(token_error(overlay).unwrap_or(Error::Timeout));
            }
        }
        ran?;
        if length == 0 {
            return Ok(0);
        }
        let left = remaining(self.memory.read32(data_qtd + QTD_TOKEN));
        Ok(length.saturating_sub(left))
    }

    fn write_qtd(
        &mut self,
        at: usize,
        next: Option<usize>,
        qtd_token: u32,
        buffer: u32,
    ) -> Result<(), Error> {
        let next = match next {
            Some(next) => self.address(next)?,
            None => TERMINATE,
        };
        self.memory.write32(at + QTD_NEXT, next);
        self.memory.write32(at + QTD_ALTERNATE, TERMINATE);
        self.memory.write32(at + QTD_BUFFER, buffer);
        for word in 1..5 {
            self.memory.write32(at + QTD_BUFFER + 4 * word, 0);
        }
        self.memory.barrier();
        self.memory.write32(at + QTD_TOKEN, qtd_token);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Interrupt pipes
    // -----------------------------------------------------------------------

    /// Open an interrupt IN pipe to `endpoint` of `target`, reading packets
    /// of up to `max_packet` bytes, and link it into every frame: its index.
    ///
    /// # Errors
    ///
    /// [`Error::NoPipe`] when all [`MAX_PIPES`] are open, [`Error::TooLong`]
    /// for a packet past [`PIPE_BYTES`].
    pub fn open_pipe(
        &mut self,
        target: &Target,
        endpoint: u8,
        max_packet: u16,
    ) -> Result<usize, Error> {
        let length = usize::from(max_packet);
        if length == 0 || length > PIPE_BYTES {
            return Err(Error::TooLong);
        }
        let pipe = self
            .pipes
            .iter()
            .position(Option::is_none)
            .ok_or(Error::NoPipe)?;
        let qh = pipe_qh(pipe);
        let qh_address = self.address(qh)?;
        let first = self.address(pipe_qtd(pipe, 0))?;
        for which in 0..2 {
            self.arm(pipe, which, length)?;
        }
        let (characteristics, capabilities) = endpoint_for_pipe(target, endpoint, max_packet, pipe);
        self.memory
            .write32(qh + QH_CHARACTERISTICS, characteristics);
        self.memory.write32(qh + QH_CAPABILITIES, capabilities);
        self.memory.write32(qh + QH_CURRENT, 0);
        self.memory.write32(qh + QH_NEXT, first);
        self.memory.write32(qh + QH_ALTERNATE, TERMINATE);
        self.memory.write32(qh + QH_TOKEN, 0);
        for word in 0..5 {
            self.memory.write32(qh + QH_BUFFERS + 4 * word, 0);
        }
        // Linked in after the anchor: this head first points where the
        // anchor did, and only then does the anchor point here.
        let after = self.memory.read32(ANCHOR_QH + QH_LINK);
        self.memory.write32(qh + QH_LINK, after);
        self.memory.barrier();
        self.memory
            .write32(ANCHOR_QH + QH_LINK, qh_address | TYPE_QH);
        self.memory.barrier();
        if let Some(slot) = self.pipes.get_mut(pipe) {
            *slot = Some(Pipe {
                next: 0,
                length,
                halts: 0,
            });
        }
        Ok(pipe)
    }

    /// Take pipe `pipe` out of the schedule, and wait until the controller
    /// can no longer be reading it.
    pub fn close_pipe(&mut self, pipe: usize) {
        if self.pipes.get(pipe).copied().flatten().is_none() {
            return;
        }
        let qh = pipe_qh(pipe);
        let Ok(address) = self.address(qh) else {
            return;
        };
        let target = address | TYPE_QH;
        let following = self.memory.read32(qh + QH_LINK);
        // Its predecessor: the anchor, or another pipe's head.
        let mut previous = ANCHOR_QH;
        for _ in 0..=MAX_PIPES {
            let link = self.memory.read32(previous + QH_LINK);
            if link == target {
                self.memory.write32(previous + QH_LINK, following);
                break;
            }
            match self.offset_of(link & !0x1F) {
                Some(next) if link & TERMINATE == 0 => previous = next,
                _ => break,
            }
        }
        self.memory.barrier();
        // A frame's walk may already have passed the predecessor and be
        // about to read this head: two frames, and it has finished.
        self.clock.sleep_nanos(2 * MILLISECOND);
        if let Some(slot) = self.pipes.get_mut(pipe) {
            *slot = None;
        }
    }

    /// The offset of the queue head whose address is `address`, among the
    /// anchor and the pipes'.
    fn offset_of(&self, address: u32) -> Option<usize> {
        core::iter::once(ANCHOR_QH)
            .chain((0..MAX_PIPES).map(pipe_qh))
            .find(|&offset| self.address(offset).ok() == Some(address))
    }

    /// Arm one of a pipe's qTDs: its buffer, its link to the other, and
    /// last its token, active.
    fn arm(&mut self, pipe: usize, which: usize, length: usize) -> Result<(), Error> {
        let buffer = self.address(pipe_buffer(pipe, which))?;
        let bytes = u32::try_from(length).map_err(|_| Error::TooLong)?;
        self.write_qtd(
            pipe_qtd(pipe, which),
            Some(pipe_qtd(pipe, 1 - which)),
            token(PID_IN, bytes, false, true),
            buffer,
        )
    }

    /// Acknowledge the controller's interrupt and collect every packet its
    /// pipes finished, in order, handing each to `receive`.
    ///
    /// # Errors
    ///
    /// [`Error::Unresponsive`] when the controller reported a system error
    /// and halted.
    pub fn service(&mut self, mut receive: impl FnMut(Packet)) -> Result<(), Error> {
        let status = self.read(USBSTS);
        self.write(USBSTS, status & STS_ACKNOWLEDGE);
        if status & STS_SYSTEM_ERROR != 0 {
            return Err(Error::Unresponsive);
        }
        for pipe in 0..MAX_PIPES {
            // Two at most: the controller cannot finish a qTD that has not
            // been armed again.
            for _ in 0..2 {
                match self.take(pipe) {
                    Some(packet) => receive(packet),
                    None => break,
                }
            }
        }
        Ok(())
    }

    /// The next finished qTD of `pipe`, read and armed again.
    fn take(&mut self, pipe: usize) -> Option<Packet> {
        let state = self.pipes.get(pipe).copied().flatten()?;
        let qtd = pipe_qtd(pipe, state.next);
        let qtd_token = self.memory.read32(qtd + QTD_TOKEN);
        if qtd_token & TOKEN_ACTIVE != 0 {
            return None;
        }
        if token_error(qtd_token).is_some() {
            self.recover(pipe, state);
            return None;
        }
        let len = state
            .length
            .saturating_sub(remaining(qtd_token))
            .min(PIPE_BYTES);
        let mut packet = Packet {
            pipe,
            bytes: [0; PIPE_BYTES],
            len,
        };
        let buffer = pipe_buffer(pipe, state.next);
        for (at, slot) in packet.bytes.iter_mut().enumerate().take(len) {
            *slot = self.memory.read8(buffer + at);
        }
        self.arm(pipe, state.next, state.length).ok()?;
        if let Some(Some(slot)) = self.pipes.get_mut(pipe) {
            slot.next = 1 - state.next;
            slot.halts = 0;
        }
        Some(packet)
    }

    /// Start a pipe that halted again, from its first qTD, up to
    /// [`MAX_HALTS`] times in a row. The controller leaves a halted queue
    /// head alone, so its overlay may be rewritten.
    fn recover(&mut self, pipe: usize, state: Pipe) {
        let halts = state.halts + 1;
        if let Some(Some(slot)) = self.pipes.get_mut(pipe) {
            slot.halts = halts;
            slot.next = 0;
        }
        if halts > MAX_HALTS {
            return;
        }
        let qh = pipe_qh(pipe);
        let (Ok(first), Ok(()), Ok(())) = (
            self.address(pipe_qtd(pipe, 0)),
            self.arm(pipe, 0, state.length),
            self.arm(pipe, 1, state.length),
        ) else {
            return;
        };
        self.memory.write32(qh + QH_CURRENT, 0);
        self.memory.write32(qh + QH_NEXT, first);
        self.memory.write32(qh + QH_ALTERNATE, TERMINATE);
        self.memory.barrier();
        // Last, and clearing Halted and the toggle: the device's endpoint
        // was reset by nothing, but a halted pipe is one whose next packet
        // is a fresh start either way.
        self.memory.write32(qh + QH_TOKEN, 0);
        self.memory.barrier();
    }

    /// Whether pipe `pipe` has halted for good.
    #[must_use]
    pub fn pipe_stopped(&self, pipe: usize) -> bool {
        self.pipes
            .get(pipe)
            .copied()
            .flatten()
            .is_some_and(|state| state.halts > MAX_HALTS)
    }

    // -----------------------------------------------------------------------
    // Plumbing
    // -----------------------------------------------------------------------

    fn address(&self, offset: usize) -> Result<u32, Error> {
        self.memory.device_address(offset).ok_or(Error::Memory)
    }

    fn read(&self, register: u32) -> u32 {
        self.registers.read32(self.operational + register)
    }

    fn write(&mut self, register: u32, value: u32) {
        self.registers.write32(self.operational + register, value);
    }

    /// Wait until `register & mask == value`.
    fn wait_register(&mut self, register: u32, mask: u32, value: u32, limit: u64) -> bool {
        let registers = &self.registers;
        let at = self.operational + register;
        wait_for(&mut self.clock, limit, POLL_STEP, || {
            registers.read32(at) & mask == value
        })
    }
}

/// The rest after `SET_ADDRESS`: two milliseconds by the specification,
/// ten as Linux gives.
const ADDRESS_RECOVERY: u64 = 10 * MILLISECOND;

impl<R: Registers, D: Dma, C: Clock> Host for Controller<R, D, C> {
    type Clock = C;
    type Parts = Parts<R, D, C>;

    fn clock(&mut self) -> &mut C {
        Controller::clock(self)
    }

    fn shutdown(self) -> Result<Parts<R, D, C>, Parts<R, D, C>> {
        Controller::shutdown(self)
    }

    fn ports(&self) -> u32 {
        Controller::ports(self)
    }

    fn root_port(&self, port: u32) -> RootPort {
        Controller::root_port(self, port)
    }

    fn clear_root_changes(&mut self, port: u32) {
        Controller::clear_root_changes(self, port);
    }

    fn reset_root_port(&mut self, port: u32) -> RootReset {
        Controller::reset_root_port(self, port)
    }

    /// Nothing to do: a device in the default state answers at address
    /// zero, and the control queue head is written for each transfer.
    fn open(&mut self, place: &Place) -> Result<Target, Error> {
        Ok(Target {
            address: 0,
            speed: place.speed,
            max_packet: place.speed.default_max_packet(),
            translator: place.translator,
            slot: 0,
            route: place.route,
            root_port: place.root_port,
        })
    }

    /// `SET_ADDRESS`, and the rest the device is owed after it.
    fn assign(&mut self, target: &mut Target, address: u8, max_packet: u16) -> Result<(), Error> {
        let _ = self.control(target, &Setup::set_address(address), Data::None)?;
        self.clock.sleep_nanos(ADDRESS_RECOVERY);
        target.address = address;
        target.max_packet = max_packet;
        Ok(())
    }

    fn control(&mut self, target: &Target, setup: &Setup, data: Data<'_>) -> Result<usize, Error> {
        Controller::control(self, target, setup, data)
    }

    /// Every pipe is polled every frame, whatever interval it asks for.
    fn open_pipe(&mut self, target: &Target, endpoint: &Endpoint) -> Result<usize, Error> {
        Controller::open_pipe(self, target, endpoint.number, endpoint.max_packet)
    }

    fn close_pipe(&mut self, pipe: usize) {
        Controller::close_pipe(self, pipe);
    }

    fn pipe_stopped(&self, pipe: usize) -> bool {
        Controller::pipe_stopped(self, pipe)
    }

    fn service(&mut self, receive: impl FnMut(Packet)) -> Result<(), Error> {
        Controller::service(self, receive)
    }
}

/// A qTD token: active, three tries.
const fn token(pid: u32, bytes: u32, toggle: bool, interrupt: bool) -> u32 {
    let mut value =
        TOKEN_ACTIVE | TOKEN_TRIES | pid | ((bytes & TOKEN_BYTES_MASK) << TOKEN_BYTES_SHIFT);
    if toggle {
        value |= TOKEN_TOGGLE;
    }
    if interrupt {
        value |= TOKEN_IOC;
    }
    value
}

/// The bytes a token has left to move.
const fn remaining(token: u32) -> usize {
    ((token >> TOKEN_BYTES_SHIFT) & TOKEN_BYTES_MASK) as usize
}

/// The error a token ended in, if it halted.
const fn token_error(token: u32) -> Option<Error> {
    if token & TOKEN_HALTED == 0 {
        None
    } else if token & TOKEN_BABBLE != 0 {
        Some(Error::Babble)
    } else if token & TOKEN_BUFFER_ERROR != 0 {
        Some(Error::Buffer)
    } else if token & TOKEN_TRANSACTION != 0 {
        Some(Error::Transaction)
    } else {
        Some(Error::Stall)
    }
}

const fn speed_bits(speed: Speed) -> u32 {
    match speed {
        Speed::Full => EPS_FULL,
        Speed::Low => EPS_LOW,
        // No device faster than high speed is ever on this bus.
        Speed::High | Speed::Super => EPS_HIGH,
    }
}

/// A queue head's endpoint characteristics and capabilities for a control
/// endpoint (`control`) or an interrupt one, without the masks.
fn endpoint(target: &Target, number: u8, max_packet: u16, control: bool) -> (u32, u32) {
    let mut characteristics = u32::from(target.address & 0x7F)
        | (u32::from(number & 0xF) << 8)
        | (speed_bits(target.speed) << 12)
        | (u32::from(max_packet & 0x7FF) << 16);
    let high = target.speed == Speed::High;
    if control {
        // The toggle is the qTDs' to say, so SETUP starts at DATA0 and the
        // data and status stages at DATA1. This queue head is the only one
        // in the asynchronous list, so it is its head.
        characteristics |= DATA_TOGGLE_FROM_QTD | HEAD_OF_LIST;
        if high {
            characteristics |= NAK_RELOAD << 28;
        } else {
            characteristics |= CONTROL_ENDPOINT;
        }
    }
    let mut capabilities = ONE_PER_MICROFRAME;
    if !high && let Some((hub, port)) = target.translator {
        capabilities |= (u32::from(hub & 0x7F) << 16) | (u32::from(port & 0x7F) << 23);
    }
    (characteristics, capabilities)
}

/// An interrupt pipe's characteristics and capabilities, with the masks
/// that schedule it every frame. Pipe `slot` starts in its own microframe
/// -- the first four for a split transaction, whose completes follow two to
/// four microframes later and must stay inside the frame -- so the pipes
/// behind one transaction translator do not all ask it for bus time in the
/// same microframe: a low-speed packet takes eight times a full-speed one's,
/// and four split starts in microframe 0 left the last pipe's completes
/// missing their window on the DK board, for good.
fn endpoint_for_pipe(target: &Target, number: u8, max_packet: u16, slot: usize) -> (u32, u32) {
    let (characteristics, mut capabilities) = endpoint(target, number, max_packet, false);
    if target.speed == Speed::High {
        capabilities |= HIGH_SPEED_START << (slot % 8);
    } else {
        let start = (slot % 4) as u32;
        capabilities |= (SPLIT_START << start) | (SPLIT_COMPLETE << (8 + start));
    }
    (characteristics, capabilities)
}

const _: () = assert!(
    QH_WORDS * 4 <= QH_SLOT && QTD_WORDS * 4 <= QTD_SLOT,
    "descriptors fit their slots"
);
