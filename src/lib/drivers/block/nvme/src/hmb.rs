//! The Host Memory Buffer: memory the host lends a controller that has no
//! DRAM of its own, where it keeps a cache of its logical-to-physical map
//! (NVMe Base Specification 2.0, section 5.27.1.10 and figure 275's
//! `HMPRE`, `HMMIN`, `HMMINDS` and `HMMAXD`).
//!
//! The controller says how much it would like (`HMPRE`) and the least it
//! can use (`HMMIN`), both in 4 KiB units, and may bound the pieces: none
//! smaller than `HMMINDS` units, no more than `HMMAXD` of them. The host
//! describes what it lends in a *descriptor list*, sixteen bytes a piece --
//! an eight-byte address, a four-byte size in pages, four reserved -- and
//! names the list in Set Features 0Dh.
//!
//! A controller must work without the buffer; it is then slower, since
//! every map lookup its small internal memory misses is a read of flash.
//! So a buffer below `HMMIN`, or one the controller refuses, is no buffer,
//! and the drive is used without.
//!
//! The memory is the controller's until host memory is disabled or the
//! controller is reset. [`crate::Driver`] disables it before a shutdown,
//! and gives the same memory back after a reset with the memory-return bit.

use crate::Memory;
use crate::regs::PAGE;

/// Bytes of one descriptor.
pub const DESCRIPTOR_BYTES: usize = 16;
/// The most pieces this crate describes: one page of descriptors.
pub const MAX_CHUNKS: usize = PAGE / DESCRIPTOR_BYTES;
/// The most host memory lent, in pages: 128 MiB, the bound Linux uses.
pub const LIMIT_PAGES: u32 = 32 * 1024;

/// One piece of host memory: `pages` pages the device sees as one run from
/// `address`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Chunk {
    /// The device address of its first byte, page aligned.
    pub address: u64,
    /// Its pages.
    pub pages: u32,
}

/// What the controller asks of a host memory buffer, bounded by what this
/// host lends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Wish {
    /// Pages to lend if they can be had.
    pub preferred_pages: u32,
    /// The least that is of use.
    pub minimum_pages: u32,
    /// No piece may be smaller.
    pub min_chunk_pages: u32,
    /// No more pieces than this.
    pub max_chunks: usize,
}

impl Wish {
    /// What a controller reporting these Identify fields asks for, lending
    /// at most `limit_pages`. `None` when it uses no host memory, or needs
    /// more than the limit.
    #[must_use]
    pub fn of(
        preferred: u32,
        minimum: u32,
        min_chunk: u32,
        max_chunks: u16,
        limit_pages: u32,
    ) -> Option<Wish> {
        if preferred == 0 || minimum > limit_pages {
            return None;
        }
        let max_chunks = match usize::from(max_chunks) {
            0 => MAX_CHUNKS,
            most => most.min(MAX_CHUNKS),
        };
        Some(Wish {
            preferred_pages: preferred.min(limit_pages).max(minimum),
            minimum_pages: minimum,
            min_chunk_pages: min_chunk.max(1),
            max_chunks,
        })
    }

    /// Whether `chunks` is a buffer the controller can use: every piece
    /// large enough and page aligned, not too many, and at least the
    /// minimum together.
    #[must_use]
    pub fn accepts(&self, chunks: &[Chunk]) -> bool {
        let total: u64 = chunks.iter().map(|chunk| u64::from(chunk.pages)).sum();
        !chunks.is_empty()
            && chunks.len() <= self.max_chunks
            && total >= u64::from(self.minimum_pages)
            && u32::try_from(total).is_ok()
            && chunks.iter().all(|chunk| {
                chunk.pages >= self.min_chunk_pages && chunk.address.is_multiple_of(PAGE as u64)
            })
    }
}

/// Write the descriptor list for `chunks` into `memory` from `offset`; the
/// pages they hold together.
pub fn write_descriptors<M: Memory>(memory: &mut M, offset: usize, chunks: &[Chunk]) -> u32 {
    let mut pages = 0_u32;
    for (index, chunk) in chunks.iter().take(MAX_CHUNKS).enumerate() {
        let at = offset + index * DESCRIPTOR_BYTES;
        let address = chunk.address.to_le_bytes();
        let size = chunk.pages.to_le_bytes();
        let reserved = [0_u8; 4];
        let bytes = address.iter().chain(size.iter()).chain(reserved.iter());
        for (byte_index, byte) in bytes.enumerate() {
            memory.write_u8(at + byte_index, *byte);
        }
        pages = pages.saturating_add(chunk.pages);
    }
    pages
}
