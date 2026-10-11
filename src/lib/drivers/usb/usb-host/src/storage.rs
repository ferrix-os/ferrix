//! A USB mass-storage disk: the bulk-only transport (USB Mass Storage Class
//! Bulk-Only Transport, revision 1.0) carrying the SCSI commands a disk
//! needs -- `INQUIRY`, `TEST UNIT READY`, `REQUEST SENSE`, `READ CAPACITY
//! (10)`, `READ (10)`, `WRITE (10)` and `SYNCHRONIZE CACHE (10)`.
//!
//! # The transport
//!
//! Every command is three steps over the interface's two bulk endpoints: a
//! 31-byte command block wrapper out, the data in or out, and a 13-byte
//! command status wrapper in. The wrappers carry a tag, which must come
//! back, and the status wrapper says how much of the data did not move and
//! whether the command passed, failed, or left the two sides out of step. A
//! failed command is asked for its sense, so the next one starts clean; a
//! phase error, a wrapper that makes no sense, or a transfer that failed
//! outright is answered with the transport's reset: the class request, and
//! the halt cleared on both endpoints.
//!
//! # What is not here
//!
//! Logical units past the first, disks of more than 2^32 blocks (`READ
//! CAPACITY (16)`), removable media that is not there, and the UAS
//! transport.

use crate::host::{self, Data, Endpoint, Host, Target};
use crate::usb::{Interface, Setup};
use crate::{Clock, MILLISECOND};

/// The mass-storage interface class.
pub const CLASS_STORAGE: u8 = 0x08;
/// Its subclass for a transparent SCSI command set.
pub const SUBCLASS_SCSI: u8 = 0x06;
/// Its protocol for the bulk-only transport.
pub const PROTOCOL_BULK_ONLY: u8 = 0x50;

/// `dCBWSignature`, "USBC", and `dCSWSignature`, "USBS", as they lie in
/// memory.
const COMMAND_SIGNATURE: [u8; 4] = *b"USBC";
const STATUS_SIGNATURE: [u8; 4] = *b"USBS";
const COMMAND_BYTES: usize = 31;
const STATUS_BYTES: usize = 13;
/// `bmCBWFlags`: the data goes to the host.
const FLAG_IN: u8 = 0x80;
/// `bCSWStatus`.
const STATUS_PASSED: u8 = 0;
const STATUS_FAILED: u8 = 1;

const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;

/// The class request that resets the transport.
const MASS_STORAGE_RESET: u8 = 0xFF;

/// The most one command moves: 64 KiB, which every disk takes.
pub const COMMAND_DATA: usize = 64 * 1024;
/// The most one bulk transfer moves, which every [`Host`] with bulk pipes
/// takes.
const TRANSFER_BYTES: usize = 4096;
/// How often a disk that is not ready yet is asked again, and how long
/// between: five seconds in all, for a disk still spinning up.
const READY_TRIES: u32 = 50;
const READY_STEP: u64 = 100 * MILLISECOND;

/// Why a disk command failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A transfer failed.
    Transfer(host::Error),
    /// The disk failed the command, with this sense key.
    Failed(u8),
    /// The two sides were out of step, and the transport was reset.
    Phase,
    /// The interface has no bulk pair, or the disk's geometry is one this
    /// driver does not serve.
    Unsupported,
    /// The request reaches past the disk's end, or is not whole blocks.
    Range,
}

impl From<host::Error> for Error {
    fn from(error: host::Error) -> Self {
        Error::Transfer(error)
    }
}

/// A disk behind a bulk-only interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Disk {
    target: Target,
    interface: u8,
    pair: usize,
    /// The endpoints' addresses, for the reset.
    address_in: u8,
    address_out: u8,
    tag: u32,
    /// How many blocks it has.
    pub blocks: u64,
    /// How many bytes a block is.
    pub block_bytes: u32,
    /// `INQUIRY`'s vendor and product, space-padded as the disk gave them.
    pub vendor: [u8; 8],
    /// The product.
    pub product: [u8; 16],
}

/// Whether `interface` is one [`Disk::open`] drives.
#[must_use]
pub fn is_disk(interface: &Interface) -> bool {
    interface.class == CLASS_STORAGE
        && interface.subclass == SUBCLASS_SCSI
        && interface.protocol == PROTOCOL_BULK_ONLY
        && interface.bulk_in.is_some()
        && interface.bulk_out.is_some()
}

impl Disk {
    /// Open the disk behind `interface` of `target`: its bulk pair, then
    /// what it is, whether it is ready, and how large.
    ///
    /// # Errors
    ///
    /// A disk that would not answer, was not ready in time, or has a
    /// geometry this driver does not serve. The bulk pair is closed again.
    pub fn open<H: Host>(
        hc: &mut H,
        target: &Target,
        interface: &Interface,
    ) -> Result<Disk, Error> {
        let (Some((number_in, packet_in)), Some((number_out, packet_out))) =
            (interface.bulk_in, interface.bulk_out)
        else {
            return Err(Error::Unsupported);
        };
        let endpoint = |number, max_packet| Endpoint {
            number,
            max_packet,
            interval: 0,
        };
        let pair = hc.open_bulk(
            target,
            &endpoint(number_in, packet_in),
            &endpoint(number_out, packet_out),
        )?;
        let mut disk = Disk {
            target: *target,
            interface: interface.number,
            pair,
            address_in: number_in | 0x80,
            address_out: number_out,
            tag: 0,
            blocks: 0,
            block_bytes: 0,
            vendor: [b' '; 8],
            product: [b' '; 16],
        };
        let found = disk.identify(hc);
        if found.is_err() {
            hc.close_bulk(pair);
        }
        found.map(|()| disk)
    }

    fn identify<H: Host>(&mut self, hc: &mut H) -> Result<(), Error> {
        let mut inquiry = [0_u8; 36];
        let got = self.command(hc, &[INQUIRY, 0, 0, 0, 36, 0], Data::In(&mut inquiry))?;
        if got >= 32
            && let (Some(vendor), Some(product)) = (inquiry.get(8..16), inquiry.get(16..32))
        {
            self.vendor.copy_from_slice(vendor);
            self.product.copy_from_slice(product);
        }
        // A disk just powered answers "not ready" or "unit attention" until
        // it is: each failure's sense is read by `command`, which is what
        // clears it.
        let mut ready = Err(Error::Failed(0));
        for _ in 0..READY_TRIES {
            ready = self
                .command(hc, &[TEST_UNIT_READY, 0, 0, 0, 0, 0], Data::None)
                .map(drop);
            if !matches!(ready, Err(Error::Failed(_))) {
                break;
            }
            hc.clock().sleep_nanos(READY_STEP);
        }
        ready?;
        let mut capacity = [0_u8; 8];
        let got = self.command(
            hc,
            &[READ_CAPACITY_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Data::In(&mut capacity),
        )?;
        let [l0, l1, l2, l3, b0, b1, b2, b3] = capacity;
        let last = u32::from_be_bytes([l0, l1, l2, l3]);
        let block_bytes = u32::from_be_bytes([b0, b1, b2, b3]);
        // All ones means "ask with READ CAPACITY (16)", which is not here.
        let whole = block_bytes as usize <= TRANSFER_BYTES
            && block_bytes >= 512
            && block_bytes.is_power_of_two();
        if got < 8 || last == u32::MAX || !whole {
            return Err(Error::Unsupported);
        }
        self.blocks = u64::from(last) + 1;
        self.block_bytes = block_bytes;
        Ok(())
    }

    /// Close the disk's bulk pair.
    pub fn close<H: Host>(self, hc: &mut H) {
        hc.close_bulk(self.pair);
    }

    /// The device the disk is on.
    #[must_use]
    pub const fn target(&self) -> &Target {
        &self.target
    }

    /// Read whole blocks from `lba` into `buffer`.
    ///
    /// # Errors
    ///
    /// [`Error::Range`] for a buffer that is not whole blocks or reaches
    /// past the end; otherwise what the first failing command ended in.
    pub fn read<H: Host>(&mut self, hc: &mut H, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
        self.check(lba, buffer.len())?;
        let mut lba = lba;
        for chunk in buffer.chunks_mut(self.command_bytes()) {
            let count = chunk.len() / self.block_bytes as usize;
            let command = Self::transfer_command(READ_10, lba, count);
            let wanted = chunk.len();
            if self.command(hc, &command, Data::In(chunk))? != wanted {
                return Err(Error::Phase);
            }
            lba += count as u64;
        }
        Ok(())
    }

    /// Write whole blocks from `buffer` at `lba`.
    ///
    /// # Errors
    ///
    /// As [`Disk::read`].
    pub fn write<H: Host>(&mut self, hc: &mut H, lba: u64, buffer: &[u8]) -> Result<(), Error> {
        self.check(lba, buffer.len())?;
        let mut lba = lba;
        for chunk in buffer.chunks(self.command_bytes()) {
            let count = chunk.len() / self.block_bytes as usize;
            let command = Self::transfer_command(WRITE_10, lba, count);
            if self.command(hc, &command, Data::Out(chunk))? != chunk.len() {
                return Err(Error::Phase);
            }
            lba += count as u64;
        }
        Ok(())
    }

    /// Have the disk write what it holds in its cache.
    ///
    /// # Errors
    ///
    /// What the command ended in.
    pub fn flush<H: Host>(&mut self, hc: &mut H) -> Result<(), Error> {
        self.command(
            hc,
            &[SYNCHRONIZE_CACHE_10, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            Data::None,
        )
        .map(drop)
    }

    fn check(&self, lba: u64, bytes: usize) -> Result<(), Error> {
        let block = self.block_bytes as usize;
        if block == 0 || !bytes.is_multiple_of(block) {
            return Err(Error::Range);
        }
        let end = lba
            .checked_add((bytes / block) as u64)
            .ok_or(Error::Range)?;
        if end > self.blocks {
            return Err(Error::Range);
        }
        Ok(())
    }

    /// The most bytes one command moves: whole blocks within
    /// [`COMMAND_DATA`].
    const fn command_bytes(&self) -> usize {
        let block = self.block_bytes as usize;
        COMMAND_DATA / block * block
    }

    /// A `READ (10)` or `WRITE (10)` of `count` blocks at `lba`.
    fn transfer_command(operation: u8, lba: u64, count: usize) -> [u8; 10] {
        let [l0, l1, l2, l3] = (lba as u32).to_be_bytes();
        let [c0, c1] = (count as u16).to_be_bytes();
        [operation, 0, l0, l1, l2, l3, 0, c0, c1, 0]
    }

    /// Run one SCSI command: how many bytes of data moved.
    ///
    /// A command the disk failed has its sense read, which is what lets the
    /// next command through, and answers [`Error::Failed`] with the key.
    fn command<H: Host>(
        &mut self,
        hc: &mut H,
        block: &[u8],
        data: Data<'_>,
    ) -> Result<usize, Error> {
        match self.transport(hc, block, data) {
            Err(Error::Failed(_)) => {
                let mut sense = [0_u8; 18];
                let key = self
                    .transport(hc, &[REQUEST_SENSE, 0, 0, 0, 18, 0], Data::In(&mut sense))
                    .ok()
                    .and_then(|_| sense.get(2).copied())
                    .map_or(0, |byte| byte & 0x0F);
                Err(Error::Failed(key))
            }
            other => other,
        }
    }

    /// The three steps of the transport, and its reset when they fall out
    /// of step.
    fn transport<H: Host>(
        &mut self,
        hc: &mut H,
        block: &[u8],
        data: Data<'_>,
    ) -> Result<usize, Error> {
        let outcome = self.steps(hc, block, data);
        if matches!(outcome, Err(Error::Phase | Error::Transfer(_))) {
            self.reset(hc);
        }
        outcome
    }

    fn steps<H: Host>(&mut self, hc: &mut H, block: &[u8], data: Data<'_>) -> Result<usize, Error> {
        self.tag = self.tag.wrapping_add(1);
        let length = data.len();
        let mut wrapper = [0_u8; COMMAND_BYTES];
        let inward = matches!(data, Data::In(_));
        let fields = COMMAND_SIGNATURE
            .into_iter()
            .chain(self.tag.to_le_bytes())
            .chain((length as u32).to_le_bytes())
            .chain([if inward { FLAG_IN } else { 0 }, 0, block.len() as u8])
            .chain(block.iter().copied());
        for (slot, byte) in wrapper.iter_mut().zip(fields) {
            *slot = byte;
        }
        let _ = hc.bulk(self.pair, Data::Out(&wrapper))?;

        // The data, a transfer at a time; a short one ends it. A stall
        // here is the disk refusing the data, and its status still follows.
        let (moved, stage) = match data {
            Data::None => (0, Ok(())),
            Data::In(buffer) => self.data_in(hc, buffer),
            Data::Out(buffer) => self.data_out(hc, buffer),
        };
        if let Err(error) = stage
            && error != host::Error::Stall
        {
            return Err(Error::Transfer(error));
        }

        // The status; asked for once more if the endpoint stalled, as the
        // transport's specification has it (section 6.7.2).
        let mut status = [0_u8; STATUS_BYTES];
        let got = match hc.bulk(self.pair, Data::In(&mut status)) {
            Err(host::Error::Stall) => hc.bulk(self.pair, Data::In(&mut status))?,
            other => other?,
        };
        let [s0, s1, s2, s3, t0, t1, t2, t3, r0, r1, r2, r3, verdict] = status;
        if got != STATUS_BYTES
            || [s0, s1, s2, s3] != STATUS_SIGNATURE
            || u32::from_le_bytes([t0, t1, t2, t3]) != self.tag
        {
            return Err(Error::Phase);
        }
        let residue = u32::from_le_bytes([r0, r1, r2, r3]) as usize;
        match verdict {
            STATUS_PASSED => Ok(moved.min(length.saturating_sub(residue))),
            STATUS_FAILED => Err(Error::Failed(0)),
            _ => Err(Error::Phase),
        }
    }

    /// The data stage towards the host: how much moved, and how it ended.
    fn data_in<H: Host>(&self, hc: &mut H, buffer: &mut [u8]) -> (usize, Result<(), host::Error>) {
        let mut moved = 0;
        for chunk in buffer.chunks_mut(TRANSFER_BYTES) {
            let wanted = chunk.len();
            let got = match hc.bulk(self.pair, Data::In(chunk)) {
                Ok(got) => got,
                Err(error) => return (moved, Err(error)),
            };
            moved += got;
            // A short transfer ends the data.
            if got < wanted {
                break;
            }
        }
        (moved, Ok(()))
    }

    /// The data stage towards the disk.
    fn data_out<H: Host>(&self, hc: &mut H, buffer: &[u8]) -> (usize, Result<(), host::Error>) {
        let mut moved = 0;
        for chunk in buffer.chunks(TRANSFER_BYTES) {
            match hc.bulk(self.pair, Data::Out(chunk)) {
                Ok(sent) => moved += sent,
                Err(error) => return (moved, Err(error)),
            }
        }
        (moved, Ok(()))
    }

    /// Reset recovery (the transport's section 5.3.4): the class request,
    /// then the halt cleared on the IN and the OUT endpoint.
    fn reset<H: Host>(&mut self, hc: &mut H) {
        let reset = Setup {
            // Class, to the interface, host to device.
            request_type: 0x21,
            request: MASS_STORAGE_RESET,
            value: 0,
            index: u16::from(self.interface),
            length: 0,
        };
        let _ = hc.control(&self.target, &reset, Data::None);
        for address in [self.address_in, self.address_out] {
            let _ = hc.control(
                &self.target,
                &Setup::clear_endpoint_halt(address),
                Data::None,
            );
        }
    }
}
