//! The Identify data structures, read out of the page the controller filled
//! (NVMe Base Specification 2.0, figures 275 and 276; NVM Command Set
//! Specification, figure 97). Only the fields this crate acts on.
//!
//! Offsets into Identify Controller:
//!
//! ```text
//!   0  2  VID             263  1  NPSS             336  2  HMMAXD
//!   4 20  SN              265  1  APSTA            512  1  SQES
//!  24 40  MN              272  4  HMPRE            513  1  CQES
//!  64  8  FR              276  4  HMMIN            516  4  NN
//!  77  1  MDTS            332  4  HMMINDS          520  2  ONCS
//!  80  4  VER             258  1  ACL              525  1  VWC
//!  88  4  RTD3E
//! ```
//!
//! and into Identify Namespace: `NSZE` at 0, `NLBAF` at 25, `FLBAS` at 26,
//! `DPS` at 29, `NSATTR` at 99, and the LBA formats, four bytes each, from
//! 128 (`MS` in bytes 0 and 1, `LBADS` in byte 2).

use crate::Memory;

/// Bytes of a serial number.
pub const SERIAL_BYTES: usize = 20;
/// Bytes of a model number.
pub const MODEL_BYTES: usize = 40;
/// Bytes of a firmware revision.
pub const FIRMWARE_BYTES: usize = 8;

/// The bytes at `offset` of `memory`.
fn bytes<M: Memory, const N: usize>(memory: &M, offset: usize) -> [u8; N] {
    let mut out = [0_u8; N];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = memory.read_u8(offset + index);
    }
    out
}

fn le16<M: Memory>(memory: &M, offset: usize) -> u16 {
    u16::from_le_bytes(bytes(memory, offset))
}

fn le32<M: Memory>(memory: &M, offset: usize) -> u32 {
    u32::from_le_bytes(bytes(memory, offset))
}

fn le64<M: Memory>(memory: &M, offset: usize) -> u64 {
    u64::from_le_bytes(bytes(memory, offset))
}

/// What Identify Controller said, of what this crate acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Controller {
    /// The PCI vendor id the controller reports.
    pub vendor: u16,
    /// The serial number, ASCII, space padded.
    pub serial: [u8; SERIAL_BYTES],
    /// The model number, ASCII, space padded.
    pub model: [u8; MODEL_BYTES],
    /// The firmware revision, ASCII, space padded.
    pub firmware: [u8; FIRMWARE_BYTES],
    /// `MDTS`: the largest transfer is `2^MDTS` minimum pages; zero is no
    /// limit.
    pub max_transfer_shift: u8,
    /// `VER`: the specification version, major in bits 31:16, minor in 15:8.
    pub version: u32,
    /// `RTD3E`: how long a shutdown may take, in microseconds; zero when
    /// the controller does not say.
    pub shutdown_micros: u32,
    /// `ACL`: Abort commands the controller takes at once, less one.
    pub abort_limit: u8,
    /// `NPSS`: power states supported, less one.
    pub power_states: u8,
    /// `APSTA` bit 0: autonomous power state transitions are supported.
    pub apst: bool,
    /// `HMPRE`: the host memory buffer the controller would like, in 4 KiB
    /// units. Zero: it uses none.
    pub host_memory_preferred: u32,
    /// `HMMIN`: the least host memory buffer it can use, in 4 KiB units.
    pub host_memory_minimum: u32,
    /// `HMMINDS`: the smallest piece of it, in 4 KiB units; zero is no
    /// limit.
    pub host_memory_min_chunk: u32,
    /// `HMMAXD`: the most pieces; zero is no limit.
    pub host_memory_max_chunks: u16,
    /// `SQES`: the required submission entry size's log2 in bits 3:0, the
    /// largest's in 7:4.
    pub sq_entry_sizes: u8,
    /// `CQES`, the same for completion entries.
    pub cq_entry_sizes: u8,
    /// `NN`: the highest namespace id.
    pub namespaces: u32,
    /// `VWC` bit 0: a volatile write cache is present.
    pub write_cache: bool,
}

impl Controller {
    /// Read the structure out of the page at `offset` of `memory`.
    #[must_use]
    pub fn parse<M: Memory>(memory: &M, offset: usize) -> Self {
        Controller {
            vendor: le16(memory, offset),
            serial: bytes(memory, offset + 4),
            model: bytes(memory, offset + 24),
            firmware: bytes(memory, offset + 64),
            max_transfer_shift: memory.read_u8(offset + 77),
            version: le32(memory, offset + 80),
            shutdown_micros: le32(memory, offset + 88),
            abort_limit: memory.read_u8(offset + 258),
            power_states: memory.read_u8(offset + 263),
            apst: memory.read_u8(offset + 265) & 1 != 0,
            host_memory_preferred: le32(memory, offset + 272),
            host_memory_minimum: le32(memory, offset + 276),
            host_memory_min_chunk: le32(memory, offset + 332),
            host_memory_max_chunks: le16(memory, offset + 336),
            sq_entry_sizes: memory.read_u8(offset + 512),
            cq_entry_sizes: memory.read_u8(offset + 513),
            namespaces: le32(memory, offset + 516),
            write_cache: memory.read_u8(offset + 525) & 1 != 0,
        }
    }

    /// Whether entries of `2^shift` bytes are within what `sizes`, an
    /// `SQES` or `CQES` byte, allows.
    #[must_use]
    pub const fn entry_size_fits(sizes: u8, shift: u8) -> bool {
        (sizes & 0xF) <= shift && shift <= (sizes >> 4)
    }

    /// The specification version as `(major, minor)`.
    #[must_use]
    pub const fn version_pair(&self) -> (u16, u8) {
        ((self.version >> 16) as u16, (self.version >> 8) as u8)
    }
}

/// Why a namespace is not one this crate serves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NamespaceRefused {
    /// It has no blocks: the id is not an active namespace.
    Inactive,
    /// Its logical block is not 512 to 4096 bytes.
    BlockSize(u8),
    /// Its format carries metadata, which this crate has nowhere to put.
    Metadata(u16),
    /// End-to-end protection is on.
    Protected,
}

/// What Identify Namespace said, checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Namespace {
    /// Its id.
    pub id: u32,
    /// `NSZE`: logical blocks.
    pub blocks: u64,
    /// The log2 of a logical block's bytes: 9 for 512, 12 for 4096.
    pub block_shift: u8,
    /// `NSATTR` bit 0: currently write protected.
    pub read_only: bool,
}

impl Namespace {
    /// Read namespace `id`'s structure out of the page at `offset` of
    /// `memory`, and check it is one this crate serves.
    ///
    /// # Errors
    ///
    /// [`NamespaceRefused`].
    pub fn parse<M: Memory>(memory: &M, offset: usize, id: u32) -> Result<Self, NamespaceRefused> {
        let blocks = le64(memory, offset);
        if blocks == 0 {
            return Err(NamespaceRefused::Inactive);
        }
        let formats = memory.read_u8(offset + 25);
        let flbas = memory.read_u8(offset + 26);
        // Bits 3:0 index the format; bits 6:5 extend the index when the
        // namespace has more than sixteen formats.
        let mut index = usize::from(flbas & 0xF);
        if formats > 15 {
            index |= usize::from((flbas >> 5) & 3) << 4;
        }
        let format = offset + 128 + index * 4;
        let metadata = le16(memory, format);
        let shift = memory.read_u8(format + 2);
        if !(9..=12).contains(&shift) {
            return Err(NamespaceRefused::BlockSize(shift));
        }
        if metadata != 0 {
            return Err(NamespaceRefused::Metadata(metadata));
        }
        if memory.read_u8(offset + 29) & 7 != 0 {
            return Err(NamespaceRefused::Protected);
        }
        Ok(Namespace {
            id,
            blocks,
            block_shift: shift,
            read_only: memory.read_u8(offset + 99) & 1 != 0,
        })
    }

    /// Bytes in a logical block.
    #[must_use]
    pub const fn block_bytes(&self) -> u32 {
        1 << self.block_shift
    }
}

/// The first namespace id in the active list in the page at `offset`.
#[must_use]
pub fn first_active<M: Memory>(memory: &M, offset: usize) -> Option<u32> {
    match le32(memory, offset) {
        0 => None,
        id => Some(id),
    }
}
