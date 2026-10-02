//! The output buffer, which is also the match window.
//!
//! btrfs decompresses an extent whole, so every byte a match can refer to is
//! already in the caller's buffer, and the buffer is the window: there is no
//! history to keep elsewhere, and a match reaching before its start is simply
//! corrupt. Every write checks two limits — the end of the buffer, and the end
//! of the current block, which may produce at most [`BLOCK_SIZE_MAX`] bytes.

use super::{BLOCK_SIZE_MAX, ensure};

/// The output buffer, and how much of it the frame has written.
#[derive(Debug)]
pub(super) struct Window<'o> {
    buffer: &'o mut [u8],
    position: usize,
    /// Where the current block must stop.
    limit: usize,
}

impl<'o> Window<'o> {
    /// An empty window over `buffer`.
    pub(super) fn new(buffer: &'o mut [u8]) -> Self {
        Window {
            buffer,
            position: 0,
            limit: 0,
        }
    }

    /// Bytes written so far.
    pub(super) fn position(&self) -> usize {
        self.position
    }

    /// Open a block: it may write up to [`BLOCK_SIZE_MAX`] bytes, and never
    /// past the buffer.
    pub(super) fn begin_block(&mut self) {
        self.limit = self
            .position
            .saturating_add(BLOCK_SIZE_MAX)
            .min(self.buffer.len());
    }

    /// Reserve `len` bytes, returning where they start.
    fn reserve(&self, len: usize) -> Option<(usize, usize)> {
        let end = self.position.checked_add(len)?;
        ensure(end <= self.limit)?;
        Some((self.position, end))
    }

    /// Append `bytes`.
    pub(super) fn push(&mut self, bytes: &[u8]) -> Option<()> {
        // NOALLOC: `Window::reserve` claims room in the caller's buffer; this crate has no `alloc`.
        let (start, end) = self.reserve(bytes.len())?;
        self.buffer.get_mut(start..end)?.copy_from_slice(bytes);
        self.position = end;
        Some(())
    }

    /// Append `len` copies of `byte`.
    pub(super) fn fill(&mut self, byte: u8, len: usize) -> Option<()> {
        // NOALLOC: `Window::reserve` claims room in the caller's buffer; this crate has no `alloc`.
        let (start, end) = self.reserve(len)?;
        self.buffer.get_mut(start..end)?.fill(byte);
        self.position = end;
        Some(())
    }

    /// Append `len` bytes copied from `offset` bytes back.
    ///
    /// A match may overlap itself — offset 1 repeats one byte — so it is copied
    /// in rounds that each double the run already written: every round's source
    /// lies wholly before its destination, which is what lets each round be a
    /// single non-overlapping copy.
    pub(super) fn copy_match(&mut self, offset: usize, len: usize) -> Option<()> {
        ensure(offset != 0)?;
        let source = self.position.checked_sub(offset)?;
        // NOALLOC: `Window::reserve` claims room in the caller's buffer; this crate has no `alloc`.
        let (mut written, end) = self.reserve(len)?;
        while written < end {
            let (before, after) = self.buffer.split_at_mut_checked(written)?;
            let round = (written - source).min(end - written);
            let from = before.get(source..source.checked_add(round)?)?;
            after.get_mut(..round)?.copy_from_slice(from);
            written = written.checked_add(round)?;
        }
        self.position = end;
        Some(())
    }
}
