//! The output buffer, which is also the back-reference window.
//!
//! An extent is decompressed whole into a buffer the caller sized to at least
//! `ram_bytes`, so every byte a well-formed stream can refer back to is
//! already in that buffer. There is no separate 32 KiB window to maintain and
//! no second copy of anything: a distance is valid exactly when it lands at or
//! after the first byte written, and a write is valid exactly when it fits.

/// The caller's buffer and how much of it has been produced.
pub(super) struct Output<'a> {
    /// Everything this extent may expand into.
    buf: &'a mut [u8],
    /// Bytes produced so far; `buf[..len]` is the window.
    len: usize,
}

impl<'a> Output<'a> {
    /// Start writing at the front of `buf`.
    pub(super) const fn new(buf: &'a mut [u8]) -> Self {
        Output { buf, len: 0 }
    }

    /// How many bytes have been produced.
    pub(super) const fn written(&self) -> usize {
        self.len
    }

    /// Append one literal byte, or `None` if the buffer is full.
    pub(super) fn push(&mut self, byte: u8) -> Option<()> {
        *self.buf.get_mut(self.len)? = byte;
        self.len = self.len.checked_add(1)?;
        Some(())
    }

    /// Append `bytes` verbatim, or `None` if they do not all fit.
    pub(super) fn extend(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.buf.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    /// Append `length` bytes copied from `distance` bytes back.
    ///
    /// `None` for a zero distance, a distance reaching before the start of the
    /// buffer, or a copy that does not fit. The source may overlap what is
    /// being written — a distance of 1 and a length of 258 is a run of one
    /// byte — so the copy goes in chunks: the first is at most `distance`
    /// long, and each chunk makes the already-repeated region longer, so the
    /// next may be twice the size. A long run is a handful of slice copies
    /// rather than one bounds check per byte.
    pub(super) fn copy_back(&mut self, distance: usize, length: usize) -> Option<()> {
        if distance == 0 {
            return None;
        }
        let from = self.len.checked_sub(distance)?;
        let end = self.len.checked_add(length)?;
        if end > self.buf.len() {
            return None;
        }
        let mut at = self.len;
        while at < end {
            let (done, free) = self.buf.split_at_mut_checked(at)?;
            // `at > from` and `end > at` inside the loop, and the source range
            // `from..from + chunk` stays below `at`, inside `done`.
            let chunk = (at - from).min(end - at);
            free.get_mut(..chunk)?
                .copy_from_slice(done.get(from..)?.get(..chunk)?);
            at += chunk;
        }
        self.len = end;
        Some(())
    }
}
