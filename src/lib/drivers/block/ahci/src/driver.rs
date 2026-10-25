//! The HBA's life: the BIOS handoff, AHCI enable, finding the drive,
//! IDENTIFY DEVICE, polled reads, writes and flushes, recovering a port
//! after an error or a timeout, and the clean shutdown.

use crate::command::{
    self, COMMAND_LIST, FLUSH_CACHE_EXT, IDENTIFY, IDENTIFY_PAGE, MAX_SECTORS, PRD_ENTRIES,
    PRD_MAX, READ_DMA_EXT, RECEIVED_FIS, TABLE, TABLE_FIS, TABLE_PRDT, WRITE_DMA_EXT,
};
use crate::identify::{self, Refused};
use crate::regs::{self, PAGE, port};
use crate::{Clock, Memory, Registers};

/// Pages of control memory [`Ahci::init`] needs: the command list, the
/// received FIS area and the command table in the first, IDENTIFY's data in
/// the second.
pub const CONTROL_PAGES: usize = 2;

/// How long one command may take.
pub const COMMAND_TIMEOUT: u64 = 10_000_000_000;
/// How long the port's engines may take to stop (AHCI 1.3.1 section 10.1.2).
pub const STOP_TIMEOUT: u64 = 500_000_000;
/// How often a running command is looked at, for its first
/// [`COMMAND_FINE`] nanoseconds: a SATA read of 128 KiB takes a few hundred
/// microseconds, and a poll every millisecond, as every other wait settles
/// into, added up to a millisecond to every command that ran past its first
/// one.
pub const COMMAND_POLL: u64 = 20_000;
/// How long a command is polled every [`COMMAND_POLL`] before every
/// millisecond.
pub const COMMAND_FINE: u64 = 100_000_000;
/// How long a device may stay busy after its port receives FISes again.
pub const BUSY_TIMEOUT: u64 = 1_000_000_000;
/// How long the BIOS may keep the HBA after the OS asked for it.
pub const HANDOFF_TIMEOUT: u64 = 2_000_000_000;
/// How long links get to come up after a spin-up.
pub const LINK_TIMEOUT: u64 = 100_000_000;
/// How long `PxSCTL.DET` is held at 1 for a COMRESET: AHCI asks for 1 ms.
pub const COMRESET_HOLD: u64 = 2_000_000;
/// How long a link is given to come up after a COMRESET.
pub const COMRESET_LINK_TIMEOUT: u64 = 1_000_000_000;
/// How many times a read, write or flush is issued before its failure is
/// answered: once, and twice more after a failure worth trying again
/// (`worth_retrying`), as Linux's libata retries a command its error
/// handler recovered the link for. A second [`COMMAND_TIMEOUT`] ends it
/// sooner, so a hung drive is answered in about 20 s, inside the 30 s the
/// kernel waits for a request (`src/kernel/src/interfaces/block_ring`).
pub const ATTEMPTS: u32 = 3;

/// One piece of a transfer's buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment {
    /// Its device address; even.
    pub addr: u64,
    /// Its length in bytes; even.
    pub len: u32,
}

/// What a [`Ahci::transfer`] does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// READ DMA EXT: from the disk into the segments.
    Read,
    /// WRITE DMA EXT: from the segments onto the disk.
    Write,
    /// FLUSH CACHE EXT: the drive's write cache onto the medium.
    Flush,
}

/// The drive found.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Identity {
    /// Logical sectors.
    pub sectors: u64,
    /// Bytes a logical sector.
    pub sector_size: u32,
    /// The model number, ASCII padded with spaces.
    pub model: [u8; 40],
    /// The serial number, ASCII padded with spaces.
    pub serial: [u8; 20],
    /// The HBA port it is on.
    pub port: u8,
}

/// Why something failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// No implemented port has an ATA disk with its link up.
    NoDrive,
    /// What was waited for, and did not happen in time.
    Timeout(&'static str),
    /// The device ended the command with `ERR` set: its status and error
    /// registers.
    Device {
        /// `PxTFD` status.
        status: u8,
        /// `PxTFD` error.
        error: u8,
    },
    /// The HBA reported a host bus or interface error (`PxIS`).
    Hba(u32),
    /// The segments are so fragmented that one command table cannot hold
    /// a single sector of them.
    TooLarge,
    /// The sectors asked for run past the disk's end.
    OutOfRange,
    /// The segments do not cover exactly `count` sectors.
    BadBuffer,
    /// A segment's address or length is odd, or its address is above
    /// 4 GiB and the HBA reaches only 32 bits.
    Unaligned,
    /// Fewer than [`CONTROL_PAGES`] pages, or a page the HBA cannot reach.
    ControlMemory,
    /// IDENTIFY DEVICE's data cannot be used, and words 0 to 3, 49, 60,
    /// 61, 83, 86, 100, 101 and 106 of what came back.
    Identify(Refused, [u16; 12]),
    /// The HBA's registers read as all ones: it left the bus.
    Gone,
}

/// One AHCI HBA, driving the first ATA disk on it.
#[derive(Debug)]
pub struct Ahci<R: Registers, C: Clock, M: Memory> {
    regs: R,
    clock: C,
    control: M,
    cap: u32,
    port: u8,
    identity: Identity,
    /// Whether a command [`Ahci::start_read`] launched has not been waited
    /// for.
    outstanding: bool,
    /// The first failure a command was issued again after since
    /// [`Ahci::take_retried`] last looked, and the attempt it was done on
    /// (0 when it never was).
    retried: Option<(Error, u32)>,
    /// Set when a command timed out again after its link was reset, and
    /// cleared by the next command that completes: while set, a command is
    /// issued once, so a drive that has stopped answering costs every
    /// request one [`COMMAND_TIMEOUT`], as before retries, not two.
    unresponsive: bool,
}

/// Why a port was passed over.
enum PortError {
    NotAta,
    Failed(Error),
}

impl<R: Registers, C: Clock, M: Memory> Ahci<R, C, M> {
    /// Take the HBA from the BIOS, enable AHCI mode with interrupts off,
    /// find the first implemented port with an ATA disk, point it at the
    /// control memory, start it and IDENTIFY the disk.
    pub fn init(regs: R, clock: C, control: M) -> Result<Self, Error> {
        let mut hba = Ahci {
            regs,
            clock,
            control,
            cap: 0,
            port: 0,
            identity: Identity {
                sectors: 0,
                sector_size: 512,
                model: [b' '; 40],
                serial: [b' '; 20],
                port: 0,
            },
            outstanding: false,
            retried: None,
            unresponsive: false,
        };
        hba.cap = hba.regs.read(regs::CAP);
        if hba.cap == regs::GONE {
            return Err(Error::Gone);
        }
        let pages = hba.control.pages();
        if pages.len() < CONTROL_PAGES
            || pages
                .iter()
                .take(CONTROL_PAGES)
                .any(|&page| page % PAGE as u64 != 0 || !hba.reachable(page + PAGE as u64 - 1))
        {
            return Err(Error::ControlMemory);
        }
        hba.handoff();
        let ghc = hba.regs.read(regs::GHC);
        hba.regs.write(
            regs::GHC,
            (ghc | regs::GHC_AE) & !(regs::GHC_IE | regs::GHC_HR),
        );
        let implemented = hba.regs.read(regs::PI);
        hba.spin_up(implemented);
        let mut failure = Error::NoDrive;
        for index in 0..regs::MAX_PORTS {
            if implemented & (1 << index) == 0 {
                continue;
            }
            let ssts = hba.regs.read(port(index, regs::PX_SSTS));
            let det = ssts & regs::SSTS_DET_MASK;
            // A link firmware left down or offline (DET 1 or 4) is
            // reset, as Linux does; an empty port (DET 0) is not waited for.
            if det != regs::SSTS_DET_ESTABLISHED
                && (det == 0 || hba.stop(index).is_err() || !hba.comreset(index))
            {
                continue;
            }
            match hba.try_port(index) {
                Ok(identity) => {
                    hba.identity = identity;
                    return Ok(hba);
                }
                Err(PortError::NotAta) => {}
                Err(PortError::Failed(error)) => failure = error,
            }
        }
        Err(failure)
    }

    /// The drive found.
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The first failure a command was issued again after since the last
    /// call, and the attempt that then did it, 0 when none did: for the
    /// console, so a drive or link that needed a second try is said even
    /// when the request succeeded.
    pub fn take_retried(&mut self) -> Option<(Error, u32)> {
        self.retried.take()
    }

    /// Read, write or flush, one command at a time, polled to completion.
    /// `lba` and `count` are in sectors, and `data` covers exactly
    /// `count` sectors; a flush ignores all three. Transfers longer than
    /// one command moves are split.
    ///
    /// A read [`Ahci::start_read`] launched is waited for first, and what
    /// came of it is lost: call [`Ahci::finish`] to learn it.
    pub fn transfer(
        &mut self,
        op: Op,
        lba: u64,
        count: u32,
        data: &[Segment],
    ) -> Result<(), Error> {
        let _earlier = self.finish();
        let opcode = match op {
            Op::Flush => return self.issue(FLUSH_CACHE_EXT, 0, 0, false, &[]),
            Op::Read => READ_DMA_EXT,
            Op::Write => WRITE_DMA_EXT,
        };
        self.check(lba, count, data)?;
        let size = u64::from(self.identity.sector_size);
        let mut cursor = Cursor::default();
        let mut lba = lba;
        let mut left = u64::from(count);
        while left > 0 {
            let most = left.min(u64::from(MAX_SECTORS)) * size;
            let mut pieces = [(0u64, 0u32); PRD_ENTRIES];
            let used = cursor.fill(data, most, &mut pieces);
            let bytes: u64 = pieces
                .iter()
                .take(used)
                .map(|piece| u64::from(piece.1))
                .sum();
            let sectors = bytes / size;
            if sectors == 0 {
                return Err(Error::TooLarge);
            }
            let used = trim(&mut pieces, used, bytes - sectors * size);
            let pieces = pieces.get(..used).ok_or(Error::TooLarge)?;
            self.issue(opcode, lba, sectors as u16, op == Op::Write, pieces)?;
            cursor.advance(data, sectors * size);
            lba += sectors;
            left -= sectors;
        }
        Ok(())
    }

    /// Launch one READ DMA EXT of up to `count` sectors at `lba` into
    /// `data`, which covers exactly `count` sectors, and return without
    /// waiting for it: how many sectors it reads from `lba`, fewer than
    /// `count` when `data` is too fragmented for one command table, or
    /// longer than one command moves. [`Ahci::finish`] waits for it; any
    /// other call waits for it first.
    ///
    /// So a driver reads ahead while the kernel takes what it read last:
    /// the drive works between two requests instead of after each.
    pub fn start_read(&mut self, lba: u64, count: u32, data: &[Segment]) -> Result<u32, Error> {
        let _earlier = self.finish();
        self.check(lba, count, data)?;
        let size = u64::from(self.identity.sector_size);
        let most = u64::from(count.min(MAX_SECTORS)) * size;
        let mut pieces = [(0u64, 0u32); PRD_ENTRIES];
        let used = Cursor::default().fill(data, most, &mut pieces);
        let bytes: u64 = pieces
            .iter()
            .take(used)
            .map(|piece| u64::from(piece.1))
            .sum();
        let sectors = bytes / size;
        if sectors == 0 {
            return Err(Error::TooLarge);
        }
        let used = trim(&mut pieces, used, bytes - sectors * size);
        let pieces = pieces.get(..used).ok_or(Error::TooLarge)?;
        self.launch(READ_DMA_EXT, lba, sectors as u16, false, pieces)?;
        self.outstanding = true;
        Ok(sectors as u32)
    }

    /// Wait for the read [`Ahci::start_read`] launched, and say what came of
    /// it; `Ok` when there is none.
    pub fn finish(&mut self) -> Result<(), Error> {
        if !self.outstanding {
            return Ok(());
        }
        self.outstanding = false;
        self.complete()
    }

    /// Whether `count` sectors at `lba` are on the disk and `data` is a
    /// buffer for exactly them the HBA can reach.
    fn check(&self, lba: u64, count: u32, data: &[Segment]) -> Result<(), Error> {
        let size = u64::from(self.identity.sector_size);
        let end = lba.checked_add(u64::from(count)).ok_or(Error::OutOfRange)?;
        if end > self.identity.sectors {
            return Err(Error::OutOfRange);
        }
        let total: u64 = data.iter().map(|segment| u64::from(segment.len)).sum();
        if total != u64::from(count) * size {
            return Err(Error::BadBuffer);
        }
        if data.iter().any(|segment| {
            segment.addr % 2 != 0
                || segment.len % 2 != 0
                || !self.reachable(segment.addr + u64::from(segment.len).saturating_sub(1))
        }) {
            return Err(Error::Unaligned);
        }
        Ok(())
    }

    /// Flush the drive's cache, stop the port, and hand the parts back.
    pub fn shutdown(mut self) -> (R, C, M) {
        let _earlier = self.finish();
        // A failed flush cannot be retried from here; the port stops either
        // way.
        let _flushed = self.transfer(Op::Flush, 0, 0, &[]);
        let _stopped = self.stop(self.port);
        (self.regs, self.clock, self.control)
    }

    /// Whether the HBA reaches `addr`.
    const fn reachable(&self, addr: u64) -> bool {
        self.cap & regs::CAP_S64A != 0 || addr <= u32::MAX as u64
    }

    /// The BIOS/OS handoff (AHCI 1.3.1 section 10.6.3), when the HBA has
    /// one. A BIOS that never lets go is not waited for past
    /// [`HANDOFF_TIMEOUT`].
    fn handoff(&mut self) {
        if self.regs.read(regs::CAP2) & regs::CAP2_BOH == 0 {
            return;
        }
        let bohc = self.regs.read(regs::BOHC);
        self.regs.write(regs::BOHC, bohc | regs::BOHC_OOS);
        let _owned = self.wait(HANDOFF_TIMEOUT, "BIOS handoff", |hba| {
            hba.regs.read(regs::BOHC) & (regs::BOHC_BOS | regs::BOHC_BB) == 0
        });
    }

    /// Spin up every implemented port of an HBA with staggered spin-up,
    /// and give links that see a device time to come up.
    fn spin_up(&mut self, implemented: u32) {
        if self.cap & regs::CAP_SSS == 0 {
            return;
        }
        for index in (0..regs::MAX_PORTS).filter(|i| implemented & (1 << i) != 0) {
            let cmd = self.regs.read(port(index, regs::PX_CMD));
            self.regs.write(
                port(index, regs::PX_CMD),
                cmd | regs::CMD_SUD | regs::CMD_POD,
            );
        }
        let _up = self.wait(LINK_TIMEOUT, "link", |hba| {
            (0..regs::MAX_PORTS)
                .filter(|i| implemented & (1 << i) != 0)
                .all(|i| {
                    hba.regs.read(port(i, regs::PX_SSTS)) & regs::SSTS_DET_MASK
                        != regs::SSTS_DET_PRESENT
                })
        });
    }

    /// Bring up port `index` and IDENTIFY its device.
    fn try_port(&mut self, index: u8) -> Result<Identity, PortError> {
        self.stop(index).map_err(PortError::Failed)?;
        self.port = index;
        for offset in 0..PAGE {
            self.control.write_u8(offset, 0);
        }
        let list = self.control_addr(COMMAND_LIST);
        let fis = self.control_addr(RECEIVED_FIS);
        self.regs.write(port(index, regs::PX_CLB), list as u32);
        self.regs
            .write(port(index, regs::PX_CLBU), (list >> 32) as u32);
        self.regs.write(port(index, regs::PX_FB), fis as u32);
        self.regs
            .write(port(index, regs::PX_FBU), (fis >> 32) as u32);
        self.regs.write(port(index, regs::PX_IE), 0);
        self.regs.write(port(index, regs::PX_SERR), u32::MAX);
        self.regs.write(port(index, regs::PX_IS), u32::MAX);
        let cmd = self.regs.read(port(index, regs::PX_CMD));
        self.regs
            .write(port(index, regs::PX_CMD), cmd | regs::CMD_FRE);
        let idle = |hba: &mut Self| {
            hba.wait(BUSY_TIMEOUT, "device busy", |hba| {
                hba.regs.read(port(index, regs::PX_TFD)) & (regs::TFD_BSY | regs::TFD_DRQ) == 0
            })
        };
        let mut idle = idle(self);
        // A device firmware left busy, or that sent no signature since its
        // last reset, gets a COMRESET and one more wait: what a BIOS leaves
        // behind is not what QEMU starts with.
        if (idle.is_err() || self.regs.read(port(index, regs::PX_SIG)) == regs::GONE)
            && self.comreset(index)
        {
            idle = self.wait(BUSY_TIMEOUT, "device busy after COMRESET", |hba| {
                hba.regs.read(port(index, regs::PX_TFD)) & (regs::TFD_BSY | regs::TFD_DRQ) == 0
            });
        }
        if let Err(error) = idle {
            let _stopped = self.stop(index);
            return Err(PortError::Failed(error));
        }
        if self.regs.read(port(index, regs::PX_SIG)) != regs::SIG_ATA {
            // Not ours to serve, but its receive engine points at our
            // memory: only a port that stopped is passed over, so that
            // `NoDrive` means nothing of the controller still writes there.
            return Err(match self.stop(index) {
                Ok(()) => PortError::NotAta,
                Err(error) => PortError::Failed(error),
            });
        }
        self.start();
        let data = self.control_addr(IDENTIFY_PAGE);
        self.issue(IDENTIFY, 0, 0, false, &[(data, 512)])
            .map_err(PortError::Failed)?;
        let parsed = identify::parse(|word| self.control.read_u16(IDENTIFY_PAGE + 2 * word))
            .map_err(|refused| {
                let mut words = [0_u16; 12];
                for (slot, word) in words
                    .iter_mut()
                    .zip([0, 1, 2, 3, 49, 60, 61, 83, 86, 100, 101, 106])
                {
                    *slot = self.control.read_u16(IDENTIFY_PAGE + 2 * word);
                }
                PortError::Failed(Error::Identify(refused, words))
            })?;
        Ok(Identity {
            sectors: parsed.sectors,
            sector_size: parsed.sector_size,
            model: parsed.model,
            serial: parsed.serial,
            port: index,
        })
    }

    /// COMRESET port `index`, whose command list is stopped: `PxSCTL.DET`
    /// to 1 for at least a millisecond, then back to 0, with Partial and
    /// Slumber off, and wait for the link (AHCI 1.3.1 section 10.4.2).
    /// Whether the link came up.
    fn comreset(&mut self, index: u8) -> bool {
        let at = port(index, regs::PX_SCTL);
        let sctl = self.regs.read(at) & !regs::SCTL_DET_IPM_MASK;
        self.regs
            .write(at, sctl | regs::SCTL_IPM_NONE | regs::SCTL_DET_INIT);
        let start = self.clock.now_nanos();
        while self.clock.now_nanos().saturating_sub(start) < COMRESET_HOLD {
            self.clock.pause(COMRESET_HOLD);
        }
        self.regs.write(at, sctl | regs::SCTL_IPM_NONE);
        let up = self
            .wait(COMRESET_LINK_TIMEOUT, "link after COMRESET", |hba| {
                hba.regs.read(port(index, regs::PX_SSTS)) & regs::SSTS_DET_MASK
                    == regs::SSTS_DET_ESTABLISHED
            })
            .is_ok();
        self.regs.write(port(index, regs::PX_SERR), u32::MAX);
        up
    }

    /// Stop port `index`'s command list and FIS receive engines.
    fn stop(&mut self, index: u8) -> Result<(), Error> {
        let at = port(index, regs::PX_CMD);
        let cmd = self.regs.read(at);
        if cmd & regs::CMD_ST != 0 {
            self.regs.write(at, cmd & !regs::CMD_ST);
        }
        self.wait(STOP_TIMEOUT, "PxCMD.CR", |hba| {
            hba.regs.read(at) & regs::CMD_CR == 0
        })?;
        let cmd = self.regs.read(at);
        if cmd & regs::CMD_FRE != 0 {
            self.regs.write(at, cmd & !regs::CMD_FRE);
        }
        self.wait(STOP_TIMEOUT, "PxCMD.FR", |hba| {
            hba.regs.read(at) & regs::CMD_FR == 0
        })
    }

    /// Start the port's command list engine.
    fn start(&mut self) {
        let at = port(self.port, regs::PX_CMD);
        let cmd = self.regs.read(at);
        self.regs.write(at, cmd | regs::CMD_ST);
    }

    /// Get the port going again after a failed or hung command (AHCI 1.3.1
    /// section 6.2.2.1): stop the command list, clear the errors, override
    /// a busy task file, and start again.
    fn recover(&mut self) {
        let at = port(self.port, regs::PX_CMD);
        let cmd = self.regs.read(at);
        self.regs.write(at, cmd & !regs::CMD_ST);
        let _stopped = self.wait(STOP_TIMEOUT, "PxCMD.CR", |hba| {
            hba.regs.read(at) & regs::CMD_CR == 0
        });
        self.regs.write(port(self.port, regs::PX_SERR), u32::MAX);
        self.regs.write(port(self.port, regs::PX_IS), u32::MAX);
        let tfd = self.regs.read(port(self.port, regs::PX_TFD));
        if tfd & (regs::TFD_BSY | regs::TFD_DRQ) != 0 && self.cap & regs::CAP_SCLO != 0 {
            let cmd = self.regs.read(at);
            self.regs.write(at, cmd | regs::CMD_CLO);
            let _overridden = self.wait(STOP_TIMEOUT, "PxCMD.CLO", |hba| {
                hba.regs.read(at) & regs::CMD_CLO == 0
            });
        }
        self.start();
    }

    /// Build slot 0's command, issue it and poll it to completion; issue it
    /// again after a failure [`worth_retrying`], up to [`ATTEMPTS`] times
    /// in all, and after a timeout once more at most, with the link reset
    /// first. A failure is answered only when the last attempt failed too.
    ///
    /// A drive that timed out again after the reset is taken to have stopped
    /// answering, and its next commands are issued once each until one
    /// completes (`unresponsive`).
    ///
    /// The port is recovered after every failure ([`Ahci::complete`]), so
    /// each attempt starts on a running command list, as the first did. A
    /// request is idempotent at the block layer, so a write done twice is
    /// only done twice.
    fn issue(
        &mut self,
        opcode: u8,
        lba: u64,
        count: u16,
        write: bool,
        pieces: &[(u64, u32)],
    ) -> Result<(), Error> {
        let mut timeouts = 0u32;
        let mut first: Option<Error> = None;
        let attempts = if self.unresponsive { 1 } else { ATTEMPTS };
        for attempt in 1..=attempts {
            self.launch(opcode, lba, count, write, pieces)?;
            let error = match self.complete() {
                Ok(()) => {
                    self.unresponsive = false;
                    if let Some(error) = first {
                        self.note_retried(error, attempt);
                    }
                    return Ok(());
                }
                Err(error) => error,
            };
            if first.is_none() {
                first = Some(error);
            }
            if matches!(error, Error::Timeout(_)) {
                timeouts += 1;
            }
            if timeouts > 1 {
                self.unresponsive = true;
            }
            if attempt == attempts || !worth_retrying(error) || timeouts > 1 {
                if attempt > 1
                    && let Some(first) = first
                {
                    self.note_retried(first, 0);
                }
                return Err(error);
            }
            // A drive that did not answer in time gets its link reset
            // before the next try, as libata's error handler resets it.
            if timeouts == 1 && matches!(error, Error::Timeout(_)) {
                self.reset_link();
            }
        }
        Err(Error::Timeout("command"))
    }

    /// Remember the first failure retried since [`Ahci::take_retried`].
    fn note_retried(&mut self, error: Error, attempt: u32) {
        if self.retried.is_none() {
            self.retried = Some((error, attempt));
        }
    }

    /// COMRESET the drive's link between two attempts at a command that
    /// timed out: the command list stopped, the link reset, the device
    /// waited for, the errors cleared and the list started again.
    fn reset_link(&mut self) {
        let at = port(self.port, regs::PX_CMD);
        let cmd = self.regs.read(at);
        self.regs.write(at, cmd & !regs::CMD_ST);
        let _stopped = self.wait(STOP_TIMEOUT, "PxCMD.CR", |hba| {
            hba.regs.read(at) & regs::CMD_CR == 0
        });
        let index = self.port;
        if self.comreset(index) {
            let _idle = self.wait(BUSY_TIMEOUT, "device busy after COMRESET", |hba| {
                hba.regs.read(port(index, regs::PX_TFD)) & (regs::TFD_BSY | regs::TFD_DRQ) == 0
            });
        }
        self.regs.write(port(index, regs::PX_SERR), u32::MAX);
        self.regs.write(port(index, regs::PX_IS), u32::MAX);
        self.start();
    }

    /// Build slot 0's command and issue it.
    fn launch(
        &mut self,
        opcode: u8,
        lba: u64,
        count: u16,
        write: bool,
        pieces: &[(u64, u32)],
    ) -> Result<(), Error> {
        let table = self.control_addr(TABLE);
        let prds = u16::try_from(pieces.len()).map_err(|_| Error::TooLarge)?;
        self.put(COMMAND_LIST, &command::header(write, prds, table));
        self.put(TABLE + TABLE_FIS, &command::fis(opcode, lba, count));
        for (i, &(addr, len)) in pieces.iter().enumerate() {
            self.put(
                TABLE + TABLE_PRDT + i * command::PRD_SIZE,
                &command::prd(addr, len),
            );
        }
        self.control.barrier();
        self.regs.write(port(self.port, regs::PX_IS), u32::MAX);
        self.regs.write(port(self.port, regs::PX_CI), 1);
        Ok(())
    }

    /// Poll slot 0's command to completion, and say what came of it.
    fn complete(&mut self) -> Result<(), Error> {
        let outcome = self.wait_paced(
            COMMAND_TIMEOUT,
            "command",
            COMMAND_FINE,
            COMMAND_POLL,
            |hba| {
                let is = hba.regs.read(port(hba.port, regs::PX_IS));
                is & regs::IS_ERRORS != 0 || hba.regs.read(port(hba.port, regs::PX_CI)) & 1 == 0
            },
        );
        if let Err(error) = outcome {
            self.recover();
            return Err(error);
        }
        // The device's last FIS ends the command, not PxCI alone: wait for the
        // task file to say it is no longer busy.
        let settled = self.wait(BUSY_TIMEOUT, "BSY after the command", |hba| {
            hba.regs.read(port(hba.port, regs::PX_TFD)) & (regs::TFD_BSY | regs::TFD_DRQ) == 0
        });
        if let Err(error) = settled {
            self.recover();
            return Err(error);
        }
        self.control.barrier();
        let is = self.regs.read(port(self.port, regs::PX_IS));
        let tfd = self.regs.read(port(self.port, regs::PX_TFD));
        if is == regs::GONE {
            return Err(Error::Gone);
        }
        if is & regs::IS_TFES != 0 || (is & regs::IS_ERRORS == 0 && tfd & regs::TFD_ERR != 0) {
            self.recover();
            return Err(Error::Device {
                status: tfd as u8,
                error: (tfd >> 8) as u8,
            });
        }
        if is & regs::IS_ERRORS != 0 {
            self.recover();
            return Err(Error::Hba(is));
        }
        Ok(())
    }

    /// Write `bytes` into control memory at `offset`.
    fn put(&mut self, offset: usize, bytes: &[u8]) {
        for (i, &byte) in bytes.iter().enumerate() {
            self.control.write_u8(offset + i, byte);
        }
    }

    /// The device address of control memory's byte `offset`.
    fn control_addr(&self, offset: usize) -> u64 {
        let page = self
            .control
            .pages()
            .get(offset / PAGE)
            .copied()
            .unwrap_or(0);
        page + (offset % PAGE) as u64
    }

    /// Poll `done` until it holds or `timeout` nanoseconds pass: every
    /// 5 us for the first millisecond, then every millisecond.
    fn wait(
        &mut self,
        timeout: u64,
        what: &'static str,
        done: impl FnMut(&mut Self) -> bool,
    ) -> Result<(), Error> {
        self.wait_paced(timeout, what, 1_000_000, 5_000, done)
    }

    /// Poll `done` until it holds or `timeout` nanoseconds pass: every
    /// `poll` nanoseconds for the first `fine`, then every millisecond.
    fn wait_paced(
        &mut self,
        timeout: u64,
        what: &'static str,
        fine: u64,
        poll: u64,
        mut done: impl FnMut(&mut Self) -> bool,
    ) -> Result<(), Error> {
        let start = self.clock.now_nanos();
        loop {
            if done(self) {
                return Ok(());
            }
            let waited = self.clock.now_nanos().saturating_sub(start);
            if waited >= timeout {
                return Err(Error::Timeout(what));
            }
            self.clock
                .pause(if waited < fine { poll } else { 1_000_000 });
        }
    }
}

/// The error register's uncorrectable-data bit (ATA8-ACS): the medium could
/// not give or take the sector.
const ERROR_UNC: u8 = 1 << 6;

/// Whether a command that failed so is worth issuing again: a timeout, an
/// HBA's host bus or interface error, or a device error other than an
/// uncorrectable medium error. Linux's libata retries a read or write after
/// every failure but a medium error (`ata_eh_worth_retry`), and so does
/// this; a drive that has gone, and a request this driver refused before
/// issuing it, are not.
const fn worth_retrying(error: Error) -> bool {
    match error {
        Error::Timeout(_) | Error::Hba(_) => true,
        Error::Device { error, .. } => error & ERROR_UNC == 0,
        _ => false,
    }
}

/// Where in a transfer's segments the next command starts.
#[derive(Clone, Copy, Default, Debug)]
struct Cursor {
    segment: usize,
    offset: u32,
}

impl Cursor {
    /// Cut up to `most` bytes from here into PRDT pieces of at most
    /// [`PRD_MAX`] bytes; how many pieces were filled.
    fn fill(self, data: &[Segment], most: u64, pieces: &mut [(u64, u32)]) -> usize {
        let mut at = self;
        let mut bytes = 0u64;
        let mut used = 0;
        for piece in pieces.iter_mut() {
            let Some(segment) = data.get(at.segment) else {
                break;
            };
            if bytes >= most {
                break;
            }
            let left = segment.len - at.offset;
            let take = u64::from(left.min(PRD_MAX)).min(most - bytes) as u32;
            *piece = (segment.addr + u64::from(at.offset), take);
            bytes += u64::from(take);
            used += 1;
            at.offset += take;
            if at.offset == segment.len {
                at = Cursor {
                    segment: at.segment + 1,
                    offset: 0,
                };
            }
        }
        used
    }

    /// Move on by `bytes`.
    fn advance(&mut self, data: &[Segment], mut bytes: u64) {
        while bytes > 0 {
            let Some(segment) = data.get(self.segment) else {
                return;
            };
            let left = u64::from(segment.len - self.offset);
            if bytes < left {
                self.offset += bytes as u32;
                return;
            }
            bytes -= left;
            self.segment += 1;
            self.offset = 0;
        }
    }
}

/// Drop `excess` bytes off the end of the first `used` pieces, so the
/// command moves whole sectors; how many pieces are left.
fn trim(pieces: &mut [(u64, u32)], mut used: usize, mut excess: u64) -> usize {
    while excess > 0 && used > 0 {
        let Some(last) = pieces.get_mut(used - 1) else {
            break;
        };
        if u64::from(last.1) > excess {
            last.1 -= excess as u32;
            break;
        }
        excess -= u64::from(last.1);
        used -= 1;
    }
    used
}
