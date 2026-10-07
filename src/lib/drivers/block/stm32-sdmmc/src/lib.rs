//! An STM32MP15's SDMMC controller and the SD card behind it, as logic over
//! registers: the card's bring-up, block reads and writes through the
//! controller's FIFO, and the rule that keeps writes inside the card's own
//! `ferrix-` partitions.
//!
//! `src/user/system/native/drivers/block/stm32-sdmmc` runs this in a ring-3
//! process under devmgr and serves the card over the block ring
//! (`docs/BLOCK-RING.md`). The process holds an `IoMapping` of the
//! controller's registers and a clock; neither exists in a unit test, so
//! everything that decides a register value is here, written against
//! [`Registers`], [`Clock`] and [`Words`], and tested against a model of the
//! controller and a card (`tests`).
//!
//! * [`regs`]: the register offsets and bits used;
//! * [`host`]: the controller -- taking it over from firmware, the card
//!   clock, one command, one transfer through the FIFO;
//! * [`card`]: the SD protocol -- bringing a card from idle to transfer
//!   state, reading and writing blocks, recovering from an error;
//! * [`gpt`]: the card's partition table, read only to decide where a write
//!   may go.
//!
//! # What firmware has done, and what this crate leaves alone
//!
//! On the STM32MP157D-DK1 U-Boot reads the card to boot, so by the time a
//! driver starts the controller is clocked, its pins are muxed, the card is
//! powered and the controller drives its lines (`POWER.PWRCTRL` = on). None
//! of that is touched here: no RCC clock or reset, no PWR regulator, no GPIO.
//! [`host::Controller::take_over`] refuses a controller that is not powered
//! on rather than powering it. What is written is inside the controller's
//! own window: the card clock's divider and bus width, the commands, the
//! data path, and the status flags (`regs`).
//!
//! # No DMA
//!
//! The controller has an internal DMA that writes memory at addresses the
//! driver programs, and nothing in front of SDMMC1 on this chip checks them.
//! This crate never turns it on: every word goes through the FIFO, moved by
//! the processor, with hardware flow control (`CLKCR.HWFC_EN`) stopping the
//! card's clock while the FIFO is full or empty, so a driver that is late to
//! the FIFO -- preempted, or a slow page fault -- slows the card down and
//! loses nothing. A bug here can then put wrong bytes in the driver's own
//! data region, and nothing else.
//!
//! Flow control is what Linux sets for every variant of this controller
//! (`mmci_sdmmc_set_clkreg` sets `HWFCEN` unconditionally). ST's errata
//! sheet for the STM32MP15, ES0438, was not to be had on 2026-10-07
//! (st.com timed out from both hosts that tried); reading its SDMMC
//! entries is owed before the first board run.
//!
//! # Where the numbers come from
//!
//! Register offsets and bits are RM0436's (STM32MP157 reference manual),
//! as Linux's `drivers/mmc/host/mmci.h` and U-Boot's
//! `drivers/mmc/stm32_sdmmc2.c` define them. The SD protocol -- the command
//! numbers, the R1 card status, the CSD's capacity fields, the bring-up
//! sequence -- is the SD Association's *Physical Layer Simplified
//! Specification*. Both drivers are GPL-2.0; none of their code is here.

#![no_std]
#![forbid(unsafe_code)]

pub mod card;
pub mod gpt;
pub mod host;
pub mod regs;

#[cfg(test)]
mod tests;

/// Bytes in a block, the one block size this crate moves: an SDHC or SDXC
/// card's is fixed at 512, and an SDSC card is set to it.
pub const BLOCK_BYTES: usize = 512;

/// Words in a block.
pub const BLOCK_WORDS: usize = BLOCK_BYTES / 4;

/// The controller's registers.
pub trait Registers {
    /// Read the register at `offset`.
    fn read(&self, offset: u32) -> u32;
    /// Write the register at `offset`.
    fn write(&mut self, offset: u32, value: u32);
}

/// Time, for timeouts and the waits between polls.
pub trait Clock {
    /// Nanoseconds on a clock that only moves forward.
    fn now_nanos(&self) -> u64;
    /// Called between two polls of a condition that may take a while: a
    /// card programming a block, or initialising. The process sleeps a
    /// little; a test's clock moves on.
    fn pause(&mut self);
}

/// Memory a transfer reads from or writes into, a word at a time: the block
/// ring's data region in the process, a buffer in a test.
pub trait Words {
    /// Its length in bytes.
    fn len_bytes(&self) -> usize;
    /// The little-endian word at byte `offset`, a multiple of four inside
    /// the memory.
    fn read_word(&self, offset: usize) -> u32;
    /// Write the little-endian word at byte `offset`, a multiple of four
    /// inside the memory.
    fn write_word(&mut self, offset: usize, value: u32);
}

/// A block's worth of memory on the stack, for the reads a driver makes for
/// itself: the partition table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block(pub [u32; BLOCK_WORDS]);

impl Default for Block {
    fn default() -> Self {
        Block([0; BLOCK_WORDS])
    }
}

impl Block {
    /// The byte at `index`, zero past the end.
    #[must_use]
    pub fn byte(&self, index: usize) -> u8 {
        let word = self.0.get(index / 4).copied().unwrap_or(0);
        word.to_le_bytes().get(index % 4).copied().unwrap_or(0)
    }

    /// The bytes, in order.
    #[must_use]
    pub fn bytes(&self) -> [u8; BLOCK_BYTES] {
        let mut out = [0_u8; BLOCK_BYTES];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.0.iter()) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

impl Words for Block {
    fn len_bytes(&self) -> usize {
        BLOCK_BYTES
    }

    fn read_word(&self, offset: usize) -> u32 {
        self.0.get(offset / 4).copied().unwrap_or(0)
    }

    fn write_word(&mut self, offset: usize, value: u32) {
        if let Some(word) = self.0.get_mut(offset / 4) {
            *word = value;
        }
    }
}
