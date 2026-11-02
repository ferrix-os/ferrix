//! [`Chunks`]: bytes kept in page-sized blocks, grown fallibly and charged
//! before each allocation.
//!
//! A `Vec<u8>` that grows past what one heap block can hold cannot grow at
//! all: the kernel heap serves at most one buddy block (4 MiB) per
//! allocation, and doubling a buffer of more than 2 MiB asks for more than
//! that however much memory is free. An ordinary `Vec` turns the refusal into
//! the allocation error handler, which stops the machine (FX-0008, finding
//! F-97: reading `/proc/<pid>/maps` of a process whose map text passed
//! 4 MiB). Text whose size a program controls is kept here instead: every
//! allocation is one block of [`BLOCK`] bytes or the list of blocks, so no
//! request grows with the text but the list, at a pointer's three words per
//! page, and every one is fallible.
//!
//! # Charging
//!
//! This crate knows no accounting. The caller supplies a [`Meter`], and every
//! allocation is taken from it first: a refusal allocates nothing. What was
//! taken is given back when the chunks are dropped. `()` is the meter that
//! takes everything, for a caller with nothing to charge.

use alloc::vec::Vec;

use crate::{AllocError, check};

/// Bytes in one block: a page.
pub const BLOCK: usize = 4096;

/// Where [`Chunks`] charge the heap they take, before they take it.
pub trait Meter {
    /// Charge `bytes` more, or refuse and charge nothing.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the bytes may not be taken.
    fn take(&mut self, bytes: usize) -> Result<(), AllocError>;

    /// Give back `bytes` taken before.
    fn give(&mut self, bytes: usize);
}

/// The meter that takes everything and counts nothing.
impl Meter for () {
    fn take(&mut self, _bytes: usize) -> Result<(), AllocError> {
        Ok(())
    }

    fn give(&mut self, _bytes: usize) {}
}

/// Bytes kept in page-sized blocks, appended to fallibly and read at any
/// offset, every allocation charged to `M` before it is made.
///
/// Every block but the last is full, so the block holding an offset is found
/// by division.
#[derive(Debug, Default)]
pub struct Chunks<M: Meter = ()> {
    /// The blocks, each allocated with room for exactly [`BLOCK`] bytes.
    pub(crate) blocks: Vec<Vec<u8>>,
    /// Bytes held.
    len: usize,
    /// Where the blocks and the list are charged.
    meter: M,
    /// Bytes taken from the meter and not given back: the blocks and the
    /// list's capacity.
    charged: usize,
}

impl Chunks<()> {
    /// No bytes, nothing allocated, nothing charged.
    #[must_use]
    pub const fn new() -> Chunks<()> {
        Chunks::metered(())
    }
}

impl<M: Meter> Chunks<M> {
    /// No bytes and nothing allocated, charging `meter` for what is.
    #[must_use]
    pub const fn metered(meter: M) -> Chunks<M> {
        Chunks {
            blocks: Vec::new(),
            len: 0,
            meter,
            charged: 0,
        }
    }

    /// Bytes held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether it holds no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Heap held, in bytes, which is what the meter has been charged: the
    /// blocks and the list of them.
    #[must_use]
    pub const fn footprint(&self) -> usize {
        self.charged
    }

    /// Append `bytes`, all of them or none.
    ///
    /// # Errors
    ///
    /// [`AllocError`] when the meter refuses, a block or the list cannot be
    /// allocated, or the injection policy says so. Nothing is appended then,
    /// and what was taken and allocated for the attempt is given back.
    pub fn try_extend_from_slice(&mut self, bytes: &[u8]) -> Result<(), AllocError> {
        if bytes.is_empty() {
            return Ok(());
        }
        let room = self.blocks.len().saturating_mul(BLOCK) - self.len;
        let needed = bytes.len().saturating_sub(room).div_ceil(BLOCK);
        if needed > 0 {
            self.grow_list(needed)?;
            let before = self.blocks.len();
            for _ in 0..needed {
                if let Err(error) = self.add_block() {
                    self.drop_blocks_from(before);
                    return Err(error);
                }
            }
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let Some(block) = self.blocks.get_mut(self.len / BLOCK) else {
                break;
            };
            let take = (BLOCK - block.len()).min(rest.len());
            let (now, later) = rest.split_at(take);
            // Within the block's capacity, reserved exactly: no allocation.
            block.extend_from_slice(now);
            self.len += take;
            rest = later;
        }
        Ok(())
    }

    /// Copy the bytes from `offset` into `buf`, as many as fit, and say how
    /// many: `buf`'s length, or what is left before the end, whichever is
    /// less, across as many blocks as that takes. An offset at or past the
    /// end copies none.
    #[must_use]
    pub fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        let mut copied = 0;
        let mut at = offset;
        while copied < buf.len() && at < self.len {
            let Some(block) = self.blocks.get(at / BLOCK) else {
                break;
            };
            let from = block.get(at % BLOCK..).unwrap_or_default();
            let Some(to) = buf.get_mut(copied..) else {
                break;
            };
            let count = from.len().min(to.len());
            if count == 0 {
                break;
            }
            if let (Some(to), Some(from)) = (to.get_mut(..count), from.get(..count)) {
                to.copy_from_slice(from);
            }
            copied += count;
            at += count;
        }
        copied
    }

    /// Room in the list for `more` blocks, charged first: the list grows to
    /// twice its capacity or what is needed, whichever is more, exactly.
    fn grow_list(&mut self, more: usize) -> Result<(), AllocError> {
        let len = self.blocks.len();
        let capacity = self.blocks.capacity();
        let wanted = len.checked_add(more).ok_or(AllocError)?;
        if wanted <= capacity {
            return Ok(());
        }
        let target = wanted.max(capacity.saturating_mul(2)).max(4);
        let extra = (target - capacity)
            .checked_mul(size_of::<Vec<u8>>())
            .ok_or(AllocError)?;
        self.meter.take(extra)?;
        let grown = check().and_then(|()| {
            self.blocks
                .try_reserve_exact(target - len)
                .map_err(AllocError::from)
        });
        if let Err(error) = grown {
            self.meter.give(extra);
            return Err(error);
        }
        // `Vec` records the capacity asked for, not what the allocator may
        // have rounded it to, and frees with the same layout: what is held is
        // what was charged.
        debug_assert_eq!(
            self.blocks.capacity(),
            target,
            "the list holds what was charged"
        );
        self.charged += extra;
        Ok(())
    }

    /// One more empty block with room for exactly [`BLOCK`] bytes, charged
    /// first. The list has room for it.
    fn add_block(&mut self) -> Result<(), AllocError> {
        if self.blocks.len() == self.blocks.capacity() {
            return Err(AllocError);
        }
        self.meter.take(BLOCK)?;
        let mut block = Vec::new();
        let made = check().and_then(|()| block.try_reserve_exact(BLOCK).map_err(AllocError::from));
        if let Err(error) = made {
            self.meter.give(BLOCK);
            return Err(error);
        }
        self.charged += BLOCK;
        // Within the capacity reserved by `grow_list`: no allocation.
        self.blocks.push(block);
        Ok(())
    }

    /// Give back the blocks from `index` on, which hold no bytes.
    fn drop_blocks_from(&mut self, index: usize) {
        let dropped = self.blocks.len().saturating_sub(index);
        self.blocks.truncate(index);
        self.meter.give(dropped * BLOCK);
        self.charged -= dropped * BLOCK;
    }
}

impl<M: Meter> Drop for Chunks<M> {
    fn drop(&mut self) {
        self.meter.give(self.charged);
        self.charged = 0;
    }
}
