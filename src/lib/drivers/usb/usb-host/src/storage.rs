//! A USB mass-storage disk: the bulk-only transport (USB Mass Storage Class
//! Bulk-Only Transport, revision 1.0) carrying the SCSI commands a disk
//! needs -- `INQUIRY`, `TEST UNIT READY`, `REQUEST SENSE`, `READ CAPACITY
//! (10)` and `(16)`, `READ (10)` and `(16)`, `WRITE (10)` and `(16)`, and
//! `SYNCHRONIZE CACHE (10)`. The 16-byte commands are used, as Linux's `sd`
//! uses them, for a disk of more than 2^32 blocks, whose last block `READ
//! CAPACITY (10)` cannot name.
//!
//! Every constant was read from the specifications and checked against
//! Linux 7.0's headers: the opcodes and sense keys from
//! `include/scsi/scsi_proto.h`, the transport's from
//! `include/linux/usb/storage.h` (`US_BULK_*`, `USB_SC_SCSI`,
//! `USB_PR_BULK`), the class from `include/uapi/linux/usb/ch9.h`.
//!
//! # The transport
//!
//! Every command is three steps over the interface's two bulk endpoints: a
//! 31-byte command block wrapper out, the data in or out, and a 13-byte
//! command status wrapper in. The wrappers carry a tag, which must come
//! back, and the status wrapper says how much of the data did not move and
//! whether the command passed, failed, or left the two sides out of step. A
//! failed command is asked for its sense, so the next one starts clean; a
//! phase error, a wrapper that is not valid and meaningful (section 6.3: 13
//! bytes, the signature, the tag that was sent, a residue no larger than
//! the data asked for), or a transfer that failed outright is answered with
//! the transport's reset: the class request, and the halt cleared on both
//! endpoints. A read or write that moved less than it asked for is an error
//! upward, never a success with a hole in it.
//!
//! # What is not here
//!
//! Logical units past the first, removable media that is not there, the
//! write-protect bit (`MODE SENSE`: a protected stick fails its writes with
//! `DATA PROTECT` instead), and the UAS transport.

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
/// `US_BULK_STAT_PHASE`.
const STATUS_PHASE: u8 = 2;

const TEST_UNIT_READY: u8 = 0x00;
const REQUEST_SENSE: u8 = 0x03;
const INQUIRY: u8 = 0x12;
const READ_CAPACITY_10: u8 = 0x25;
const READ_10: u8 = 0x28;
const WRITE_10: u8 = 0x2A;
const SYNCHRONIZE_CACHE_10: u8 = 0x35;
const READ_16: u8 = 0x88;
const WRITE_16: u8 = 0x8A;
/// `SERVICE ACTION IN (16)`, and its action that reads the capacity.
const SERVICE_ACTION_IN_16: u8 = 0x9E;
const SAI_READ_CAPACITY_16: u8 = 0x10;
/// The bytes of `READ CAPACITY (16)`'s answer asked for: the 32 SBC-3
/// defines.
const CAPACITY_16_BYTES: usize = 32;

/// The sense key of a medium that is not ready (SPC-4, table 49).
pub const SENSE_NOT_READY: u8 = 0x02;
/// The medium could not be read or written.
pub const SENSE_MEDIUM_ERROR: u8 = 0x03;
/// The command or its fields were refused.
pub const SENSE_ILLEGAL_REQUEST: u8 = 0x05;
/// The medium changed, or the device was reset.
pub const SENSE_UNIT_ATTENTION: u8 = 0x06;
/// The medium is write-protected.
pub const SENSE_DATA_PROTECT: u8 = 0x07;

/// The bytes of a disk's serial, as the block ring's HELLO carries it.
pub const SERIAL_BYTES: usize = 20;

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
    /// A read or write moved less data than it asked for, with a status
    /// that passed: the disk's answer is not taken.
    Short,
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
    /// The device's `iSerialNumber` string in ASCII, NUL-padded and cut at
    /// [`SERIAL_BYTES`]; all zero when it has none. Set by the bus, which
    /// reads the device's strings: what tells one stick from another of
    /// the same size when a driver started again finds a disk.
    pub serial: [u8; SERIAL_BYTES],
    /// The 16-byte reads and writes are used: the disk has more than 2^32
    /// blocks.
    wide: bool,
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
            serial: [0; SERIAL_BYTES],
            wide: false,
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
        if got < 8 {
            return Err(Error::Unsupported);
        }
        let last = u32::from_be_bytes([l0, l1, l2, l3]);
        let (last, block_bytes) = if last == u32::MAX {
            // All ones: the last block is past what ten bytes can name.
            self.capacity_16(hc)?
        } else {
            (u64::from(last), u32::from_be_bytes([b0, b1, b2, b3]))
        };
        let whole = block_bytes as usize <= TRANSFER_BYTES
            && block_bytes >= 512
            && block_bytes.is_power_of_two();
        if !whole || last == u64::MAX {
            return Err(Error::Unsupported);
        }
        self.blocks = last + 1;
        self.block_bytes = block_bytes;
        self.wide = self.blocks > 1 << 32;
        Ok(())
    }

    /// `READ CAPACITY (16)`: the last block and the block's bytes.
    fn capacity_16<H: Host>(&mut self, hc: &mut H) -> Result<(u64, u32), Error> {
        let mut answer = [0_u8; CAPACITY_16_BYTES];
        let mut command = [0_u8; 16];
        put(
            &mut command,
            0,
            &[SERVICE_ACTION_IN_16, SAI_READ_CAPACITY_16],
        );
        put(&mut command, 10, &(CAPACITY_16_BYTES as u32).to_be_bytes());
        let got = self.command(hc, &command, Data::In(&mut answer))?;
        if got < 12 {
            return Err(Error::Unsupported);
        }
        let mut last = [0_u8; 8];
        let mut bytes = [0_u8; 4];
        for (slot, byte) in last.iter_mut().chain(bytes.iter_mut()).zip(answer) {
            *slot = byte;
        }
        Ok((u64::from_be_bytes(last), u32::from_be_bytes(bytes)))
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
            let command = self.transfer_command(false, lba, count);
            let wanted = chunk.len();
            if self.command(hc, command.as_slice(), Data::In(chunk))? != wanted {
                return Err(Error::Short);
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
            let command = self.transfer_command(true, lba, count);
            if self.command(hc, command.as_slice(), Data::Out(chunk))? != chunk.len() {
                return Err(Error::Short);
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

    /// A read or write of `count` blocks at `lba`: `READ (10)` or `WRITE
    /// (10)`, or their 16-byte forms on a disk of more than 2^32 blocks.
    /// `count` is at most [`COMMAND_DATA`] over 512, so it fits both, and
    /// on a disk of at most 2^32 blocks every block's number fits 32 bits.
    fn transfer_command(&self, write: bool, lba: u64, count: usize) -> Cdb {
        let mut bytes = [0_u8; 16];
        if self.wide {
            put(&mut bytes, 0, &[if write { WRITE_16 } else { READ_16 }]);
            put(&mut bytes, 2, &lba.to_be_bytes());
            put(&mut bytes, 10, &(count as u32).to_be_bytes());
            Cdb { bytes, len: 16 }
        } else {
            put(&mut bytes, 0, &[if write { WRITE_10 } else { READ_10 }]);
            put(&mut bytes, 2, &(lba as u32).to_be_bytes());
            put(&mut bytes, 7, &(count as u16).to_be_bytes());
            Cdb { bytes, len: 10 }
        }
    }

    /// Whether the disk is read and written with the 16-byte commands.
    #[must_use]
    pub const fn is_wide(&self) -> bool {
        self.wide
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
        // Meaningful (section 6.3.2): a residue no larger than what was
        // asked, and a status the transport defines.
        let residue = u32::from_le_bytes([r0, r1, r2, r3]) as usize;
        if residue > length {
            return Err(Error::Phase);
        }
        match verdict {
            STATUS_PASSED => Ok(moved.min(length - residue)),
            STATUS_FAILED => Err(Error::Failed(0)),
            STATUS_PHASE => Err(Error::Phase),
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

/// A disk's serial as the block ring carries it, from the device's
/// `iSerialNumber` string in ASCII:
///
/// * up to [`SERIAL_BYTES`] bytes: the string, NUL-padded;
/// * longer: its first four bytes, `~`, and fifteen hexadecimal digits of
///   the FNV-1a sum of the whole string, so two serials that share a
///   20-byte prefix still differ (a cut would make them one disk);
/// * empty: all zero, which says "no serial". A driver must not take a
///   disk without one for a disk it served before.
#[must_use]
pub fn serial_of(ascii: &[u8]) -> [u8; SERIAL_BYTES] {
    let mut serial = [0_u8; SERIAL_BYTES];
    if ascii.len() <= SERIAL_BYTES {
        put(&mut serial, 0, ascii);
        return serial;
    }
    let sum = ascii.iter().fold(0xCBF2_9CE4_8422_2325_u64, |sum, &byte| {
        (sum ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3)
    });
    put(&mut serial, 0, ascii.get(..4).unwrap_or(&[]));
    put(&mut serial, 4, b"~");
    for (at, slot) in serial.iter_mut().enumerate().skip(5) {
        let shift = (SERIAL_BYTES - 1 - at) * 4;
        let nibble = ((sum >> shift) & 0xF) as u8;
        *slot = if nibble < 10 {
            b'0' + nibble
        } else {
            b'a' + nibble - 10
        };
    }
    serial
}

/// Whether `serial` says the disk has none ([`serial_of`]).
#[must_use]
pub fn has_no_serial(serial: &[u8; SERIAL_BYTES]) -> bool {
    serial.iter().all(|&byte| byte == 0)
}

/// A command block of at most 16 bytes, and how many of them it is.
#[derive(Clone, Copy, Debug)]
struct Cdb {
    bytes: [u8; 16],
    len: usize,
}

impl Cdb {
    fn as_slice(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&self.bytes)
    }
}

/// Write `bytes` into `block` from `at` on; what would fall past its end is
/// dropped, which no caller here reaches.
fn put(block: &mut [u8], at: usize, bytes: &[u8]) {
    for (slot, &byte) in block.iter_mut().skip(at).zip(bytes) {
        *slot = byte;
    }
}
