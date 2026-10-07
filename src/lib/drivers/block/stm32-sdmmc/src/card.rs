//! The SD protocol: bringing a card from idle to transfer state, reading
//! and writing blocks, and finding the transfer state again after an error.
//!
//! Only SD memory cards of version 2 or later are taken -- every SDHC and
//! SDXC card, and SDSC cards made since 2006 -- at default speed (25 MHz)
//! on four data lines and 3.3 V signalling, which is what the DK1's slot is
//! wired for (`bus-width = <4>`, no voltage switch in its tree). High speed
//! and the UHS modes are later steps.

use crate::host::{Command, Controller, DEFAULT_SPEED_HZ, Direction, Error, Response};
use crate::{BLOCK_BYTES, Block, Clock, Registers, Words};

/// R1's `OUT_OF_RANGE`.
pub const OUT_OF_RANGE: u32 = 1 << 31;
/// R1's error bits: `OUT_OF_RANGE`, `ADDRESS_ERROR`, `BLOCK_LEN_ERROR`,
/// `ERASE_SEQ_ERROR`, `ERASE_PARAM`, `WP_VIOLATION`, `LOCK_UNLOCK_FAILED`,
/// `COM_CRC_ERROR`, `ILLEGAL_COMMAND`, `CARD_ECC_FAILED`, `CC_ERROR`,
/// `ERROR`, `CSD_OVERWRITE`, `WP_ERASE_SKIP` and `AKE_SEQ_ERROR`.
pub const R1_ERRORS: u32 = 0xFDF9_8008;
/// R1's `READY_FOR_DATA`.
pub const READY_FOR_DATA: u32 = 1 << 8;
/// R1's `APP_CMD`: the card took the last command as an application one.
pub const APP_CMD: u32 = 1 << 5;
/// R1's `CURRENT_STATE`, bits 12:9.
const STATE_SHIFT: u32 = 9;
/// The transfer state.
pub const STATE_TRAN: u32 = 4;
/// The programming state.
pub const STATE_PRG: u32 = 7;

/// CMD8's argument: 2.7-3.6 V, and the check pattern `0xAA` echoed back.
const IF_COND: u32 = 0x1AA;
/// ACMD41's argument: a host that takes high-capacity cards (`HCS`), and
/// the 2.7-3.6 V window.
const OP_COND: u32 = (1 << 30) | 0x00FF_8000;
/// The OCR's power-up-done bit and card-capacity bit.
const OCR_READY: u32 = 1 << 31;
const OCR_CCS: u32 = 1 << 30;

/// How long a card may take to finish powering up after ACMD41 first asks:
/// the specification's one second.
const POWER_UP_NANOS: u64 = 1_000_000_000;
/// How long a card may stay out of the transfer state after a write: the
/// specification's 250 ms busy for a block, with a margin for a slow card's
/// housekeeping.
const READY_NANOS: u64 = 1_000_000_000;

/// The card's state in an R1 word.
#[must_use]
pub const fn state(r1: u32) -> u32 {
    (r1 >> STATE_SHIFT) & 0xF
}

/// The R1 word's error bits as an error.
///
/// # Errors
///
/// [`Error::Card`] with the word when an error bit is set.
pub fn check_r1(r1: u32) -> Result<(), Error> {
    if r1 & R1_ERRORS == 0 {
        Ok(())
    } else {
        Err(Error::Card(r1))
    }
}

/// What the card said about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// Its relative card address, which addressed commands carry in their
    /// argument's top half.
    pub rca: u16,
    /// Block addressed (SDHC, SDXC) rather than byte addressed (SDSC).
    pub high_capacity: bool,
    /// Its size in 512-byte blocks.
    pub blocks: u64,
    /// The card identification register, most significant word first: its
    /// manufacturer, name and serial number.
    pub cid: [u32; 4],
}

/// A card in the transfer state, on four lines at default speed.
#[derive(Debug)]
pub struct Card<R, C> {
    host: Controller<R, C>,
    identity: Identity,
}

impl<R: Registers, C: Clock> Card<R, C> {
    /// Bring the card behind `host` from wherever it is to the transfer
    /// state: CMD0 to idle, CMD8 and ACMD41 to ready, CMD2 and CMD3 for its
    /// identity and address, CMD9 for its size, CMD7 to select it, ACMD6 to
    /// four lines, CMD16 for 512-byte blocks on a byte-addressed card, then
    /// the default-speed clock.
    ///
    /// # Errors
    ///
    /// The step's error, or [`Error::Unsupported`] for a card of version 1
    /// or no SD memory card at all.
    pub fn init(mut host: Controller<R, C>) -> Result<Self, (Controller<R, C>, Error)> {
        match bring_up(&mut host) {
            Ok(identity) => Ok(Card { host, identity }),
            Err(error) => Err((host, error)),
        }
    }

    /// What the card said about itself.
    #[must_use]
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The controller, for a test to look at.
    #[must_use]
    pub const fn host(&self) -> &Controller<R, C> {
        &self.host
    }

    /// Give the controller back.
    #[must_use]
    pub fn into_host(self) -> Controller<R, C> {
        self.host
    }

    /// Read `count` blocks from block `first` into `memory` from byte
    /// `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] for blocks past the card's end or memory that
    /// does not hold them, or the transfer's error, after which the card
    /// has been brought back to the transfer state if it could be.
    pub fn read(
        &mut self,
        first: u64,
        count: u32,
        memory: &mut dyn Words,
        offset: usize,
    ) -> Result<(), Error> {
        self.move_blocks(Direction::Read, first, count, memory, offset)
    }

    /// Write `count` blocks from `memory` at byte `offset` to block `first`,
    /// and wait until the card has programmed them.
    ///
    /// Whether the blocks may be written at all is the caller's to decide
    /// ([`crate::gpt::Writable`]); this writes what it is given.
    ///
    /// # Errors
    ///
    /// As [`Card::read`].
    pub fn write(
        &mut self,
        first: u64,
        count: u32,
        memory: &mut dyn Words,
        offset: usize,
    ) -> Result<(), Error> {
        self.move_blocks(Direction::Write, first, count, memory, offset)
    }

    /// Read one block into a [`Block`].
    ///
    /// # Errors
    ///
    /// As [`Card::read`].
    pub fn read_block(&mut self, number: u64) -> Result<Block, Error> {
        let mut block = Block::default();
        self.read(number, 1, &mut block, 0)?;
        Ok(block)
    }

    fn move_blocks(
        &mut self,
        direction: Direction,
        first: u64,
        count: u32,
        memory: &mut dyn Words,
        offset: usize,
    ) -> Result<(), Error> {
        let end = first
            .checked_add(u64::from(count))
            .ok_or(Error::OutOfRange)?;
        let bytes = (count as usize).saturating_mul(BLOCK_BYTES);
        let held = offset
            .checked_add(bytes)
            .is_some_and(|last| last <= memory.len_bytes());
        if count == 0 || end > self.identity.blocks || !held || !offset.is_multiple_of(4) {
            return Err(Error::OutOfRange);
        }
        let address = if self.identity.high_capacity {
            first
        } else {
            first.saturating_mul(BLOCK_BYTES as u64)
        };
        let address = u32::try_from(address).map_err(|_| Error::OutOfRange)?;
        let index = match (direction, count) {
            (Direction::Read, 1) => 17,
            (Direction::Read, _) => 18,
            (Direction::Write, 1) => 24,
            (Direction::Write, _) => 25,
        };
        let command = Command::r1(index, address);
        let moved = self
            .host
            .transfer(&command, direction, count, memory, offset)
            .and_then(|response| check_r1(response[0]));
        // After a write the card programs what it took; after any error it
        // may be anywhere. Either way the next command needs it back in the
        // transfer state.
        let ready = if direction == Direction::Write || moved.is_err() {
            self.wait_transfer_state()
        } else {
            Ok(())
        };
        moved?;
        ready
    }

    /// Ask the card's status (CMD13) until it is ready for data in the
    /// transfer state.
    fn wait_transfer_state(&mut self) -> Result<(), Error> {
        let start = self.host.clock().now_nanos();
        let status = Command::r1(13, u32::from(self.identity.rca) << 16);
        loop {
            let r1 = self.host.command(&status)?[0];
            check_r1(r1 & !OUT_OF_RANGE)?;
            if state(r1) == STATE_TRAN && r1 & READY_FOR_DATA != 0 {
                return Ok(());
            }
            if !matches!(state(r1), STATE_TRAN | STATE_PRG | 5 | 6) {
                return Err(Error::State(r1));
            }
            let now = self.host.clock().now_nanos();
            if now.saturating_sub(start) > READY_NANOS {
                return Err(Error::BusyTimeout);
            }
            self.host.clock().pause();
        }
    }
}

/// The steps of [`Card::init`].
fn bring_up<R: Registers, C: Clock>(host: &mut Controller<R, C>) -> Result<Identity, Error> {
    let _ = host.set_clock(crate::host::IDENTIFY_HZ, false);
    let _ = host.command(&Command {
        index: 0,
        argument: 0,
        response: Response::None,
        busy: false,
    })?;
    // CMD8: a card of version 2 or later echoes the pattern; one of version
    // 1, or no SD memory card, does not answer.
    let echo = match host.command(&Command::r1(8, IF_COND)) {
        Ok(response) => response[0],
        Err(Error::Timeout) => return Err(Error::Unsupported),
        Err(error) => return Err(error),
    };
    if echo & 0xFFF != IF_COND {
        return Err(Error::Unsupported);
    }
    let high_capacity = power_up(host)?;
    let cid = host.command(&Command {
        index: 2,
        argument: 0,
        response: Response::Long,
        busy: false,
    })?;
    // CMD3: R6, the new address in the top half and a short status below.
    let r6 = host.command(&Command::r1(3, 0))?[0];
    if r6 & 0xE000 != 0 {
        return Err(Error::Card(r6));
    }
    let rca = (r6 >> 16) as u16;
    if rca == 0 {
        return Err(Error::State(r6));
    }
    let addressed = u32::from(rca) << 16;
    let csd = host.command(&Command {
        index: 9,
        argument: addressed,
        response: Response::Long,
        busy: false,
    })?;
    let blocks = capacity(csd).ok_or(Error::Unsupported)?;
    check_r1(
        host.command(&Command {
            index: 7,
            argument: addressed,
            response: Response::Short,
            busy: true,
        })?[0],
    )?;
    let _ = app_command(host, addressed, &Command::r1(6, 2))?;
    let _ = host.set_clock(crate::host::IDENTIFY_HZ, true);
    if !high_capacity {
        check_r1(host.command(&Command::r1(16, BLOCK_BYTES as u32))?[0])?;
    }
    let _ = host.set_clock(DEFAULT_SPEED_HZ, true);
    let r1 = host.command(&Command::r1(13, addressed))?[0];
    check_r1(r1)?;
    if state(r1) != STATE_TRAN {
        return Err(Error::State(r1));
    }
    Ok(Identity {
        rca,
        high_capacity,
        blocks,
        cid,
    })
}

/// ACMD41 until the card says it has powered up; whether it is high
/// capacity.
fn power_up<R: Registers, C: Clock>(host: &mut Controller<R, C>) -> Result<bool, Error> {
    let start = host.clock().now_nanos();
    loop {
        let ocr = app_command(
            host,
            0,
            &Command {
                index: 41,
                argument: OP_COND,
                response: Response::ShortNoCrc,
                busy: false,
            },
        )?;
        if ocr & OCR_READY != 0 {
            return Ok(ocr & OCR_CCS != 0);
        }
        let now = host.clock().now_nanos();
        if now.saturating_sub(start) > POWER_UP_NANOS {
            return Err(Error::Timeout);
        }
        host.clock().pause();
    }
}

/// CMD55 for `addressed`, then `command` as an application command; its
/// response's first word.
fn app_command<R: Registers, C: Clock>(
    host: &mut Controller<R, C>,
    addressed: u32,
    command: &Command,
) -> Result<u32, Error> {
    let r1 = host.command(&Command::r1(55, addressed))?[0];
    check_r1(r1)?;
    if r1 & APP_CMD == 0 {
        return Err(Error::State(r1));
    }
    let response = host.command(command)?[0];
    if command.response == Response::Short {
        check_r1(response)?;
    }
    Ok(response)
}

/// Bits `high..=low` of a 128-bit register given as four words, most
/// significant first, as the controller's response registers hold it.
#[must_use]
pub fn bits(register: [u32; 4], high: u32, low: u32) -> u64 {
    let whole = (u128::from(register[0]) << 96)
        | (u128::from(register[1]) << 64)
        | (u128::from(register[2]) << 32)
        | u128::from(register[3]);
    let width = high.saturating_sub(low) + 1;
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1_u64 << width) - 1
    };
    ((whole >> low) as u64) & mask
}

/// The card's size in 512-byte blocks from its CSD, `None` for a CSD
/// structure this driver does not know.
#[must_use]
pub fn capacity(csd: [u32; 4]) -> Option<u64> {
    match bits(csd, 127, 126) {
        // Version 1 (SDSC): (C_SIZE + 1) * 2^(C_SIZE_MULT + 2) blocks of
        // 2^READ_BL_LEN bytes.
        0 => {
            let size = bits(csd, 73, 62) + 1;
            let multiplier = 1_u64 << (bits(csd, 49, 47) + 2);
            let block_len = 1_u64 << bits(csd, 83, 80);
            Some(size * multiplier * block_len / BLOCK_BYTES as u64)
        }
        // Version 2 (SDHC, SDXC): (C_SIZE + 1) * 512 KiB.
        1 => Some((bits(csd, 69, 48) + 1) * 1024),
        _ => None,
    }
}
