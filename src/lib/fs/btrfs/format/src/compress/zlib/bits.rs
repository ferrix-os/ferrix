//! DEFLATE's bit order over a byte slice.
//!
//! DEFLATE packs fields from the least significant bit of each byte upwards,
//! and a multi-bit number is read low bit first. Huffman codes are the one
//! exception: they are packed starting from their *most* significant bit, so a
//! code read by this reader arrives bit-reversed. The tables in `huffman` are
//! indexed by the reversed code for exactly that reason, and nothing here
//! reverses anything.
//!
//! Bytes are loaded into a 64-bit accumulator. At the end of the input it
//! simply stops being refilled: a peek sees zeros where the missing bits would
//! be, and [`Bits::consume`] checks against the bits actually held. A Huffman
//! decode can therefore look ahead fifteen bits near the end of a stream
//! without running off it, and a code that genuinely needs bits the input does
//! not have fails at the point of use rather than decoding padding that was
//! never there.

/// A cursor over DEFLATE data, reading bits in stream order.
pub(super) struct Bits<'a> {
    /// The DEFLATE data, from just after the zlib header.
    input: &'a [u8],
    /// Index of the first byte not yet loaded into `held`.
    next: usize,
    /// Loaded bits not yet consumed, the next one in bit 0.
    held: u64,
    /// How many of `held`'s low bits are real.
    count: u32,
}

impl<'a> Bits<'a> {
    /// Start reading at the first bit of `input`.
    pub(super) const fn new(input: &'a [u8]) -> Self {
        Bits {
            input,
            next: 0,
            held: 0,
            count: 0,
        }
    }

    /// Load whole bytes until the accumulator cannot take another or the input
    /// is exhausted.
    fn refill(&mut self) {
        while self.count <= 56 {
            let Some(&byte) = self.input.get(self.next) else {
                return;
            };
            self.held |= u64::from(byte) << self.count;
            self.count += 8;
            // `next` indexes a byte that exists, so it is below `isize::MAX`.
            self.next += 1;
        }
    }

    /// The next `n` bits, `n <= 32`, without consuming them.
    ///
    /// Bits beyond the end of the input read as zero; only
    /// [`consume`](Self::consume) can tell whether they were real.
    pub(super) fn peek(&mut self, n: u32) -> u32 {
        self.refill();
        let mask = (1u64 << n.min(32)) - 1;
        (self.held & mask) as u32
    }

    /// Discard `n` bits, or `None` if fewer than `n` remain in the input.
    pub(super) fn consume(&mut self, n: u32) -> Option<()> {
        self.count = self.count.checked_sub(n)?;
        self.held = self.held.checked_shr(n).unwrap_or(0);
        Some(())
    }

    /// Read an `n`-bit number, `n <= 32`, low bit first.
    pub(super) fn take(&mut self, n: u32) -> Option<u32> {
        let value = self.peek(n);
        self.consume(n)?;
        Some(value)
    }

    /// Skip to the next byte boundary and borrow the `len` bytes that follow.
    ///
    /// Whole bytes already sitting in the accumulator are handed back to the
    /// slice first, so this sees exactly the bytes the stream has not yet
    /// consumed. A stored block is the only reader.
    pub(super) fn bytes(&mut self, len: usize) -> Option<&'a [u8]> {
        let unread = (self.count / 8) as usize;
        self.next = self.next.saturating_sub(unread);
        self.held = 0;
        self.count = 0;
        let end = self.next.checked_add(len)?;
        let bytes = self.input.get(self.next..end)?;
        self.next = end;
        Some(bytes)
    }
}
