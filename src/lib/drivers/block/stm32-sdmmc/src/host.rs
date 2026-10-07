//! The controller: taking it over from firmware, the card clock, one
//! command, and one transfer through the FIFO.
//!
//! Every register this module writes is listed in [`WRITTEN`], which the
//! driver's board-run request repeats address by address.

use crate::regs::{
    ARGR, BUSSPEED, BUSYD0, CCRCFAIL, CLKCR, CLKDIV_MASK, CMDINDEX_MASK, CMDR, CMDREND, CMDSENT,
    CMDSTOP, CMDTRANS, COMMAND_ERRORS, CPSMACT, CPSMEN, CTIMEOUT, DATA_ERRORS, DATAEND,
    DBLOCKSIZE_SHIFT, DCRCFAIL, DCTRL, DDR, DLENR, DPSMACT, DTDIR, DTIMEOUT, DTIMER,
    FIFO_HALF_WORDS, FIFOR, FIFORST, HWFC_EN, ICR, ICR_ALL, IDMACTRLR, MASKR, POWER, PWRCTRL_MASK,
    PWRCTRL_ON, PWRSAV, RESP1R, RESP2R, RESP3R, RESP4R, RXFIFOE, RXFIFOHF, RXOVERR, STAR, TXFIFOHE,
    TXUNDERR, WAITRESP_LONG, WAITRESP_NONE, WAITRESP_SHORT, WAITRESP_SHORT_NO_CRC, WIDBUS_4,
    WIDBUS_MASK,
};
use crate::{BLOCK_BYTES, BLOCK_WORDS, Clock, Registers, Words};

/// Every register offset this module ever writes, in the order a run first
/// writes them. Reads go anywhere below `regs::WINDOW`. `IDMACTRLR` is
/// written only with zero, and only when firmware left the DMA on.
pub const WRITTEN: [u32; 10] = [
    MASKR, ICR, IDMACTRLR, DCTRL, CLKCR, ARGR, CMDR, DTIMER, DLENR, FIFOR,
];

/// The card clock while a card is identified: at most 400 kHz.
pub const IDENTIFY_HZ: u64 = 400_000;
/// The card clock in default speed: at most 25 MHz.
pub const DEFAULT_SPEED_HZ: u64 = 25_000_000;

/// How long a command's path may stay active after its response or
/// timeout should have come: the controller times a response out itself
/// after 64 card clocks, so this only catches a controller that stopped.
const COMMAND_NANOS: u64 = 100_000_000;
/// How long a card may hold D0 busy after an R1b response.
const BUSY_NANOS: u64 = 1_000_000_000;
/// How long a block may take to arrive or be taken, beyond the controller's
/// own `DTIMER`: again only for a controller that stopped.
const DATA_NANOS: u64 = 2_000_000_000;
/// How long the controller may stay busy with what firmware left it doing.
const IDLE_NANOS: u64 = 10_000_000;
/// The wait after a `CLKCR` write: the manual asks for seven bus clocks
/// between two writes to it, which a microsecond covers at any bus clock.
const CLKCR_SETTLE_NANOS: u64 = 1_000;

/// What went wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Firmware left the controller not driving the card's lines
    /// (`POWER.PWRCTRL` is not on), which this driver does not change.
    NotPowered(u32),
    /// The controller stayed busy with something firmware left running.
    Busy,
    /// The kernel clock's rate is zero or unknown.
    NoClock,
    /// No response came.
    Timeout,
    /// A response's CRC was wrong.
    Crc,
    /// A data block's CRC was wrong.
    DataCrc,
    /// A block did not arrive, or the card stayed busy, within `DTIMER`.
    DataTimeout,
    /// The FIFO overflowed on a read or ran dry on a write: hardware flow
    /// control should make that impossible.
    Fifo,
    /// The controller's paths did not finish in a time that only a stopped
    /// controller exceeds.
    Stuck,
    /// The card held D0 busy too long.
    BusyTimeout,
    /// The card's status after a command had error bits set: the R1 word.
    Card(u32),
    /// The card answered in a state the protocol does not allow here: the
    /// R1 word.
    State(u32),
    /// The card is one this driver does not take: version 1 of the
    /// specification, or not an SD memory card.
    Unsupported,
    /// Blocks past the end of the card, or a transfer the memory does not
    /// hold.
    OutOfRange,
}

/// A command's response, as `WAITRESP` says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// None: CMD0.
    None,
    /// 48 bits with a CRC: R1, R6, R7.
    Short,
    /// 48 bits whose CRC field is not a CRC: R3, the OCR.
    ShortNoCrc,
    /// 136 bits: R2, the CID and CSD.
    Long,
}

/// One command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Command {
    /// The command's number.
    pub index: u8,
    /// Its argument.
    pub argument: u32,
    /// What answers it.
    pub response: Response,
    /// The card may hold D0 busy after it (R1b).
    pub busy: bool,
}

impl Command {
    /// A command with an R1 response, as most are.
    #[must_use]
    pub const fn r1(index: u8, argument: u32) -> Command {
        Command {
            index,
            argument,
            response: Response::Short,
            busy: false,
        }
    }
}

/// Which way data goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the card.
    Read,
    /// To the card.
    Write,
}

/// The controller, taken over.
#[derive(Debug)]
pub struct Controller<R, C> {
    regs: R,
    clock: C,
    /// The kernel clock's rate, which the card clock is divided from.
    kernel_hz: u64,
    /// The card clock as last set.
    card_hz: u64,
}

impl<R: Registers, C: Clock> Controller<R, C> {
    /// Take the controller over from firmware: check it drives the card's
    /// lines, wait for anything it was doing to end, mask its interrupts,
    /// clear its flags, make sure its DMA is off, and slow the card clock
    /// to [`IDENTIFY_HZ`] on one data line.
    ///
    /// # Errors
    ///
    /// [`Error::NotPowered`] for a controller firmware did not leave on,
    /// [`Error::NoClock`] without a kernel clock rate, [`Error::Busy`] for
    /// one that stays busy.
    pub fn take_over(regs: R, clock: C, kernel_hz: u64) -> Result<Self, Error> {
        if kernel_hz == 0 {
            return Err(Error::NoClock);
        }
        let power = regs.read(POWER);
        if power & PWRCTRL_MASK != PWRCTRL_ON {
            return Err(Error::NotPowered(power));
        }
        let mut host = Controller {
            regs,
            clock,
            kernel_hz,
            card_hz: 0,
        };
        let start = host.clock.now_nanos();
        while host.regs.read(STAR) & (CPSMACT | DPSMACT) != 0 {
            if host.elapsed(start) > IDLE_NANOS {
                return Err(Error::Busy);
            }
            host.clock.pause();
        }
        host.regs.write(MASKR, 0);
        host.regs.write(ICR, ICR_ALL);
        // The internal DMA is turned off if firmware left it on, and never
        // touched otherwise: a zero is the only value this crate ever
        // writes to the DMA's registers, and only here.
        if host.regs.read(IDMACTRLR) != 0 {
            host.regs.write(IDMACTRLR, 0);
        }
        host.regs.write(DCTRL, 0);
        let _ = host.set_clock(IDENTIFY_HZ, false);
        Ok(host)
    }

    /// The registers, for a test to look at.
    #[must_use]
    pub fn registers(&self) -> &R {
        &self.regs
    }

    /// The clock.
    pub fn clock(&mut self) -> &mut C {
        &mut self.clock
    }

    /// The card clock as last set, in hertz.
    #[must_use]
    pub const fn card_hz(&self) -> u64 {
        self.card_hz
    }

    /// Nanoseconds since `start`.
    fn elapsed(&self, start: u64) -> u64 {
        self.clock.now_nanos().saturating_sub(start)
    }

    /// Set the card clock to at most `hz`, on four data lines if `wide`,
    /// with hardware flow control on and the clock never gated while idle.
    /// What firmware chose for the clock's edge and the receive clock
    /// (`NEGEDGE`, `SELCLKRX`) is kept: they follow the board's wiring.
    /// Answers the rate set.
    pub fn set_clock(&mut self, hz: u64, wide: bool) -> u64 {
        let divider = clock_divider(self.kernel_hz, hz);
        let kept = self.regs.read(CLKCR)
            & !(CLKDIV_MASK | PWRSAV | WIDBUS_MASK | HWFC_EN | DDR | BUSSPEED);
        let width = if wide { WIDBUS_4 } else { 0 };
        self.regs.write(CLKCR, kept | divider | width | HWFC_EN);
        let settle = self.clock.now_nanos();
        while self.elapsed(settle) < CLKCR_SETTLE_NANOS {
            self.clock.pause();
        }
        self.card_hz = if divider == 0 {
            self.kernel_hz
        } else {
            self.kernel_hz / (2 * u64::from(divider))
        };
        self.card_hz
    }

    /// Send `command` and answer its response: four words, most significant
    /// first, the first alone meaningful for a short one.
    ///
    /// # Errors
    ///
    /// [`Error::Timeout`], [`Error::Crc`], [`Error::Stuck`], and
    /// [`Error::BusyTimeout`] for an R1b the card stays busy after.
    pub fn command(&mut self, command: &Command) -> Result<[u32; 4], Error> {
        self.issue(command, 0)?;
        self.finish_command(command)
    }

    /// Write `command` to the command path, with `extra` bits of `CMDR`.
    fn issue(&mut self, command: &Command, extra: u32) -> Result<(), Error> {
        // A command path still enabled from the last command is turned off
        // first, as the manual asks before a new command is written.
        if self.regs.read(CMDR) & CPSMEN != 0 {
            self.regs.write(CMDR, 0);
        }
        let start = self.clock.now_nanos();
        while self.regs.read(STAR) & CPSMACT != 0 {
            if self.elapsed(start) > COMMAND_NANOS {
                return Err(Error::Stuck);
            }
            self.clock.pause();
        }
        self.regs.write(ICR, COMMAND_ERRORS | CMDREND | CMDSENT);
        self.regs.write(ARGR, command.argument);
        let wait = match command.response {
            Response::None => WAITRESP_NONE,
            Response::Short => WAITRESP_SHORT,
            Response::ShortNoCrc => WAITRESP_SHORT_NO_CRC,
            Response::Long => WAITRESP_LONG,
        };
        let value = (u32::from(command.index) & CMDINDEX_MASK) | wait | CPSMEN | extra;
        self.regs.write(CMDR, value);
        Ok(())
    }

    /// Wait for the response of the command just issued, then for the end
    /// of its busy.
    fn finish_command(&mut self, command: &Command) -> Result<[u32; 4], Error> {
        let done = match command.response {
            Response::None => CMDSENT,
            _ => CMDREND,
        };
        let start = self.clock.now_nanos();
        loop {
            let status = self.regs.read(STAR);
            if status & CTIMEOUT != 0 {
                self.regs.write(ICR, COMMAND_ERRORS | CMDREND | CMDSENT);
                return Err(Error::Timeout);
            }
            if status & CCRCFAIL != 0 {
                self.regs.write(ICR, COMMAND_ERRORS | CMDREND | CMDSENT);
                return Err(Error::Crc);
            }
            if status & done != 0 {
                break;
            }
            if self.elapsed(start) > COMMAND_NANOS {
                return Err(Error::Stuck);
            }
        }
        let response = [
            self.regs.read(RESP1R),
            self.regs.read(RESP2R),
            self.regs.read(RESP3R),
            self.regs.read(RESP4R),
        ];
        self.regs.write(ICR, CMDREND | CMDSENT);
        if command.busy {
            self.wait_not_busy()?;
        }
        Ok(response)
    }

    /// Wait until the card lets D0 go after an R1b response.
    fn wait_not_busy(&mut self) -> Result<(), Error> {
        let start = self.clock.now_nanos();
        while self.regs.read(STAR) & BUSYD0 != 0 {
            if self.elapsed(start) > BUSY_NANOS {
                return Err(Error::BusyTimeout);
            }
            self.clock.pause();
        }
        Ok(())
    }

    /// Move `blocks` blocks between the card and `memory` from byte
    /// `offset`, started by `command` (CMD17, CMD18, CMD24 or CMD25) and,
    /// for more than one block, ended by CMD12. Answers the response of
    /// `command`.
    ///
    /// The data path is set up before the command, which starts it
    /// (`CMDR.CMDTRANS`). Words go through the FIFO eight at a time while
    /// its half flag is up, one at a time at the ends.
    ///
    /// # Errors
    ///
    /// Whatever the command, the data or CMD12 met. The data path is
    /// stopped and the FIFO emptied whatever happened, and on an error a
    /// multi-block transfer has been sent CMD12.
    pub fn transfer(
        &mut self,
        command: &Command,
        direction: Direction,
        blocks: u32,
        memory: &mut dyn Words,
        offset: usize,
    ) -> Result<[u32; 4], Error> {
        let bytes = (blocks as usize).saturating_mul(BLOCK_BYTES);
        let end = offset.checked_add(bytes).ok_or(Error::OutOfRange)?;
        if blocks == 0 || end > memory.len_bytes() || !offset.is_multiple_of(4) {
            return Err(Error::OutOfRange);
        }
        let length = u32::try_from(bytes).map_err(|_| Error::OutOfRange)?;
        // The data timeout, in card clocks: a quarter of a second, the
        // longest the SD specification lets a write's busy last, and
        // more than a read's 100 ms.
        let timeout = u32::try_from(self.card_hz / 4).unwrap_or(u32::MAX).max(1);
        self.regs.write(ICR, ICR_ALL);
        self.regs.write(DTIMER, timeout);
        self.regs.write(DLENR, length);
        let mut control = 9 << DBLOCKSIZE_SHIFT; // 512 bytes
        if direction == Direction::Read {
            control |= DTDIR;
        }
        self.regs.write(DCTRL, control);
        let moved = self
            .issue(command, CMDTRANS)
            .and_then(|()| self.finish_command(command))
            .and_then(|response| {
                let words = blocks as usize * BLOCK_WORDS;
                match direction {
                    Direction::Read => self.drain(memory, offset, words)?,
                    Direction::Write => self.fill(memory, offset, words)?,
                }
                self.wait_data_end()?;
                Ok(response)
            });
        let stopped = if blocks > 1 { self.stop() } else { Ok(()) };
        self.reset_data_path();
        let response = moved?;
        stopped?;
        Ok(response)
    }

    /// Read `words` words from the FIFO into `memory` from `offset`.
    fn drain(&mut self, memory: &mut dyn Words, offset: usize, words: usize) -> Result<(), Error> {
        let mut done = 0;
        let start = self.clock.now_nanos();
        while done < words {
            let status = self.regs.read(STAR);
            data_error(status)?;
            let burst = if status & RXFIFOHF != 0 && words - done >= FIFO_HALF_WORDS {
                FIFO_HALF_WORDS
            } else if status & RXFIFOE == 0 {
                1
            } else {
                if self.elapsed(start) > DATA_NANOS {
                    return Err(Error::Stuck);
                }
                continue;
            };
            for _ in 0..burst {
                let word = self.regs.read(FIFOR);
                memory.write_word(offset + done * 4, word);
                done += 1;
            }
        }
        Ok(())
    }

    /// Write `words` words from `memory` at `offset` into the FIFO.
    fn fill(&mut self, memory: &mut dyn Words, offset: usize, words: usize) -> Result<(), Error> {
        let mut done = 0;
        let start = self.clock.now_nanos();
        while done < words {
            let status = self.regs.read(STAR);
            data_error(status)?;
            if status & TXFIFOHE == 0 {
                if self.elapsed(start) > DATA_NANOS {
                    return Err(Error::Stuck);
                }
                continue;
            }
            let burst = FIFO_HALF_WORDS.min(words - done);
            for _ in 0..burst {
                let word = memory.read_word(offset + done * 4);
                self.regs.write(FIFOR, word);
                done += 1;
            }
        }
        Ok(())
    }

    /// Wait for the data path to say it moved everything.
    fn wait_data_end(&mut self) -> Result<(), Error> {
        let start = self.clock.now_nanos();
        loop {
            let status = self.regs.read(STAR);
            data_error(status)?;
            if status & DATAEND != 0 {
                return Ok(());
            }
            if self.elapsed(start) > DATA_NANOS {
                return Err(Error::Stuck);
            }
        }
    }

    /// CMD12, `STOP_TRANSMISSION`, as the data path's stop: R1b.
    fn stop(&mut self) -> Result<(), Error> {
        let stop = Command {
            index: 12,
            argument: 0,
            response: Response::Short,
            busy: true,
        };
        self.issue(&stop, CMDSTOP)?;
        let response = self.finish_command(&stop)?;
        // After a stop the card reports the error bits of the transfer it
        // ended, and OUT_OF_RANGE for a multi-block read that ran to the
        // card's last block, which is not an error of the transfer.
        crate::card::check_r1(response[0] & !crate::card::OUT_OF_RANGE)
    }

    /// Stop the data path and empty the FIFO, whatever state it is in.
    fn reset_data_path(&mut self) {
        let start = self.clock.now_nanos();
        while self.regs.read(STAR) & DPSMACT != 0 && self.elapsed(start) < IDLE_NANOS {
            self.clock.pause();
        }
        self.regs.write(DCTRL, FIFORST);
        self.regs.write(DCTRL, 0);
        self.regs.write(ICR, ICR_ALL);
    }
}

/// The error a transfer's status shows, if any.
fn data_error(status: u32) -> Result<(), Error> {
    if status & DATA_ERRORS == 0 {
        return Ok(());
    }
    Err(if status & DCRCFAIL != 0 {
        Error::DataCrc
    } else if status & DTIMEOUT != 0 {
        Error::DataTimeout
    } else if status & (RXOVERR | TXUNDERR) != 0 {
        Error::Fifo
    } else {
        Error::Stuck
    })
}

/// `CLKCR.CLKDIV` for a card clock of at most `hz` from `kernel_hz`: zero,
/// the kernel clock itself, when that is slow enough, else the smallest
/// divider that is, up to the field's largest.
#[must_use]
pub fn clock_divider(kernel_hz: u64, hz: u64) -> u32 {
    if hz >= kernel_hz {
        return 0;
    }
    let divider = kernel_hz.div_ceil(2 * hz.max(1));
    u32::try_from(divider)
        .unwrap_or(CLKDIV_MASK)
        .clamp(1, CLKDIV_MASK)
}
