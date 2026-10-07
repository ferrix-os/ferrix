//! The SDMMC controller's registers: the offsets and bits this crate uses.
//!
//! RM0436 (STM32MP157) chapter "Secure digital input/output `MultiMediaCard`
//! interface (SDMMC)" describes them; the names here are the manual's. The
//! same offsets and bits are what Linux's `mmci.h` and U-Boot's
//! `stm32_sdmmc2.c` define for this controller, which is where they were
//! checked. Nothing is copied from either: these are register facts.
//!
//! The internal DMA's registers (`IDMACTRLR` at 0x050 and the rest) are named
//! only so the driver can say it leaves the DMA off.

/// `SDMMC_POWER`: the controller's drive of the card's lines.
pub const POWER: u32 = 0x000;
/// `SDMMC_CLKCR`: the card clock, the bus width and flow control.
pub const CLKCR: u32 = 0x004;
/// `SDMMC_ARGR`: a command's argument.
pub const ARGR: u32 = 0x008;
/// `SDMMC_CMDR`: a command, and the command path's start.
pub const CMDR: u32 = 0x00C;
/// `SDMMC_RESP1R` to `SDMMC_RESP4R`: the response, most significant word
/// first.
pub const RESP1R: u32 = 0x014;
/// `SDMMC_RESP2R`.
pub const RESP2R: u32 = 0x018;
/// `SDMMC_RESP3R`.
pub const RESP3R: u32 = 0x01C;
/// `SDMMC_RESP4R`.
pub const RESP4R: u32 = 0x020;
/// `SDMMC_DTIMER`: the data and busy timeout, in card clock periods.
pub const DTIMER: u32 = 0x024;
/// `SDMMC_DLENR`: bytes the data path moves.
pub const DLENR: u32 = 0x028;
/// `SDMMC_DCTRL`: the data path's direction, mode and block size.
pub const DCTRL: u32 = 0x02C;
/// `SDMMC_STAR`: status.
pub const STAR: u32 = 0x034;
/// `SDMMC_ICR`: clears status flags.
pub const ICR: u32 = 0x038;
/// `SDMMC_MASKR`: which status flags interrupt.
pub const MASKR: u32 = 0x03C;
/// `SDMMC_IDMACTRLR`: the internal DMA's enable, which stays clear.
pub const IDMACTRLR: u32 = 0x050;
/// `SDMMC_FIFOR`: the data FIFO, sixteen words wide, read or written a word
/// at a time at this one address.
pub const FIFOR: u32 = 0x080;

/// The register window's length: everything this crate touches is below it.
pub const WINDOW: u32 = 0x400;

/// `POWER.PWRCTRL`, bits 1:0: 0b11 is power-on, the lines driven.
pub const PWRCTRL_MASK: u32 = 0b11;
/// `PWRCTRL`'s power-on value.
pub const PWRCTRL_ON: u32 = 0b11;

/// `CLKCR.CLKDIV`, bits 9:0: the card clock is the kernel clock over twice
/// this, or the kernel clock itself at zero.
pub const CLKDIV_MASK: u32 = 0x3FF;
/// `CLKCR.PWRSAV`: the clock stops while the bus is idle.
pub const PWRSAV: u32 = 1 << 12;
/// `CLKCR.WIDBUS`, bits 15:14: 0b00 one data line, 0b01 four.
pub const WIDBUS_MASK: u32 = 0b11 << 14;
/// `WIDBUS`'s four-line value.
pub const WIDBUS_4: u32 = 0b01 << 14;
/// `CLKCR.HWFC_EN`: the card clock stops while the FIFO is full on a read
/// or empty on a write, so the FIFO never overruns or underruns however
/// late the processor comes to it.
pub const HWFC_EN: u32 = 1 << 17;
/// `CLKCR.DDR`.
pub const DDR: u32 = 1 << 18;
/// `CLKCR.BUSSPEED`.
pub const BUSSPEED: u32 = 1 << 19;

/// `CMDR.CMDINDEX`, bits 5:0.
pub const CMDINDEX_MASK: u32 = 0x3F;
/// `CMDR.CMDTRANS`: the command starts the data path.
pub const CMDTRANS: u32 = 1 << 6;
/// `CMDR.CMDSTOP`: the command stops the data path (CMD12).
pub const CMDSTOP: u32 = 1 << 7;
/// `CMDR.WAITRESP`, bits 9:8: none.
pub const WAITRESP_NONE: u32 = 0b00 << 8;
/// A short response with a CRC.
pub const WAITRESP_SHORT: u32 = 0b01 << 8;
/// A short response without a CRC (R3).
pub const WAITRESP_SHORT_NO_CRC: u32 = 0b10 << 8;
/// A long response, 136 bits, with a CRC (R2).
pub const WAITRESP_LONG: u32 = 0b11 << 8;
/// `CMDR.CPSMEN`: the command path starts.
pub const CPSMEN: u32 = 1 << 12;

/// `DCTRL.DTDIR`: the card sends.
pub const DTDIR: u32 = 1 << 1;
/// `DCTRL.DBLOCKSIZE`, bits 7:4: a block is two to this power bytes.
pub const DBLOCKSIZE_SHIFT: u32 = 4;
/// `DCTRL.FIFORST`: empties the FIFO; taken only while the data path is
/// idle.
pub const FIFORST: u32 = 1 << 13;

/// `STAR.CCRCFAIL`: a response's CRC was wrong.
pub const CCRCFAIL: u32 = 1 << 0;
/// `STAR.DCRCFAIL`: a data block's CRC was wrong.
pub const DCRCFAIL: u32 = 1 << 1;
/// `STAR.CTIMEOUT`: no response came.
pub const CTIMEOUT: u32 = 1 << 2;
/// `STAR.DTIMEOUT`: data, or the end of busy, did not come in `DTIMER`.
pub const DTIMEOUT: u32 = 1 << 3;
/// `STAR.TXUNDERR`: the FIFO ran empty on a write.
pub const TXUNDERR: u32 = 1 << 4;
/// `STAR.RXOVERR`: the FIFO overflowed on a read.
pub const RXOVERR: u32 = 1 << 5;
/// `STAR.CMDREND`: a response came, its CRC good.
pub const CMDREND: u32 = 1 << 6;
/// `STAR.CMDSENT`: a command that waits for no response went out.
pub const CMDSENT: u32 = 1 << 7;
/// `STAR.DATAEND`: the data path moved all of `DLENR`.
pub const DATAEND: u32 = 1 << 8;
/// `STAR.DABORT`: the data path was stopped by CMD12.
pub const DABORT: u32 = 1 << 11;
/// `STAR.DPSMACT`: the data path is active.
pub const DPSMACT: u32 = 1 << 12;
/// `STAR.CPSMACT`: the command path is active.
pub const CPSMACT: u32 = 1 << 13;
/// `STAR.TXFIFOHE`: the FIFO has room for at least eight words.
pub const TXFIFOHE: u32 = 1 << 14;
/// `STAR.RXFIFOHF`: the FIFO holds at least eight words.
pub const RXFIFOHF: u32 = 1 << 15;
/// `STAR.RXFIFOE`: the FIFO is empty on a read.
pub const RXFIFOE: u32 = 1 << 19;
/// `STAR.BUSYD0`: the card holds D0 low, busy, after a response.
pub const BUSYD0: u32 = 1 << 20;

/// Every flag `ICR` clears: bits 11:0 and 28:21, as the manual's static
/// flags are.
pub const ICR_ALL: u32 = 0x1FE0_0FFF;

/// Every error flag of a command.
pub const COMMAND_ERRORS: u32 = CCRCFAIL | CTIMEOUT;
/// Every error flag of a transfer.
pub const DATA_ERRORS: u32 = DCRCFAIL | DTIMEOUT | TXUNDERR | RXOVERR;

/// Words the FIFO takes or gives when its half flag is up.
pub const FIFO_HALF_WORDS: usize = 8;
