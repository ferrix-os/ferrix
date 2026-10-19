//! The two bit orders zstd reads.
//!
//! Only FSE table descriptions are read *forwards*: least-significant bit of
//! the first byte first, the way a header field is. Everything that is entropy
//! coded — Huffman streams, the weights of a compressed Huffman tree, the
//! sequences bitstream — is read *backwards*. The encoder wrote those streams
//! forwards while walking its input from the end, so the decoder starts at the
//! last byte, whose highest set bit is a marker saying where the data begins,
//! and walks towards the first.
//!
//! # Reading past the start is not a panic, and not yet an error
//!
//! A backward stream legitimately asks for more bits than remain. A Huffman
//! decoder peeks `max_bits` to find a code that may be shorter than what is
//! left, and the two-state FSE decoder of Huffman weights detects its own end
//! by reading *past* the start. So [`ReverseBits`] zero-extends a short read,
//! exactly as the reference decoder's container does, and records that it
//! overflowed. Whether that is corruption is the caller's decision:
//! [`ReverseBits::finished_exactly`] is the check every stream except the
//! weights makes once it has decoded what it was told to.

/// Up to 56 bits of `bytes` starting at bit `start`, least-significant first.
///
/// Bits beyond the end of `bytes` read as zero, which is what lets both
/// readers peek near the end of a stream without a separate slow path.
fn bits_at(bytes: &[u8], start: usize, count: u32) -> u64 {
    let index = start / 8;
    let shift = start % 8;
    let word = match bytes.get(index..).and_then(|tail| tail.get(..8)) {
        Some(eight) => eight.try_into().map_or(0, u64::from_le_bytes),
        None => {
            let mut padded = [0u8; 8];
            if let Some(tail) = bytes.get(index..) {
                for (to, from) in padded.iter_mut().zip(tail) {
                    *to = *from;
                }
            }
            u64::from_le_bytes(padded)
        }
    };
    (word >> shift) & low_mask(count)
}

/// A mask of the low `count` bits.
fn low_mask(count: u32) -> u64 {
    1u64.checked_shl(count)
        .map_or(u64::MAX, |bit| bit.wrapping_sub(1))
}

/// A little-endian bit reader moving forwards, for FSE table descriptions.
///
/// Unlike [`ReverseBits`] it never reads past the end: a description that
/// needs bits its slice does not have is simply truncated.
#[derive(Debug)]
pub(super) struct ForwardBits<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> ForwardBits<'a> {
    /// Start reading at the first bit of `bytes`.
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        ForwardBits { bytes, position: 0 }
    }

    /// The next `count` (at most 32) bits without consuming them, zero-padded
    /// past the end.
    pub(super) fn peek(&self, count: u32) -> u32 {
        u32::try_from(bits_at(self.bytes, self.position, count.min(32))).unwrap_or(0)
    }

    /// Consume `count` bits, or `None` if the slice does not hold them.
    pub(super) fn skip(&mut self, count: u32) -> Option<()> {
        let end = self.position.checked_add(usize::try_from(count).ok()?)?;
        (end <= self.bytes.len().saturating_mul(8)).then_some(())?;
        self.position = end;
        Some(())
    }

    /// Read and consume `count` (at most 32) bits.
    pub(super) fn read(&mut self, count: u32) -> Option<u32> {
        let value = self.peek(count);
        self.skip(count)?;
        Some(value)
    }

    /// Whole bytes touched so far: a partly read byte counts.
    pub(super) fn consumed_bytes(&self) -> usize {
        self.position.div_ceil(8)
    }
}

/// A bit reader moving backwards from a stream's end marker.
#[derive(Debug)]
pub(super) struct ReverseBits<'a> {
    bytes: &'a [u8],
    /// Bits not yet consumed; they are the low `remaining` bits of the stream.
    remaining: usize,
    /// Whether a read has asked for more than `remaining`.
    overflowed: bool,
}

impl<'a> ReverseBits<'a> {
    /// Position a reader just below the end marker of `bytes`.
    ///
    /// `None` for an empty stream or one whose last byte is zero: either has no
    /// marker, and every encoder writes one.
    pub(super) fn new(bytes: &'a [u8]) -> Option<Self> {
        let last = *bytes.last()?;
        (last != 0).then_some(())?;
        let marker = 7usize.checked_sub(usize::try_from(last.leading_zeros()).ok()?)?;
        let whole = bytes.len().checked_sub(1)?.checked_mul(8)?;
        Some(ReverseBits {
            bytes,
            remaining: whole.checked_add(marker)?,
            overflowed: false,
        })
    }

    /// The next `count` (at most 56) bits without consuming them, the first
    /// bit read being the most significant. Past the start, zeros.
    pub(super) fn peek(&self, count: u32) -> u64 {
        let count = count.min(56);
        let wanted = usize::try_from(count).unwrap_or(56);
        match self.remaining.checked_sub(wanted) {
            Some(start) => bits_at(self.bytes, start, count),
            None => {
                // `remaining < wanted <= 56`, so both conversions are exact and
                // the shift is below 64.
                let have = u32::try_from(self.remaining).unwrap_or(0);
                bits_at(self.bytes, 0, have) << count.saturating_sub(have)
            }
        }
    }

    /// Consume `count` bits, noting an overflow if fewer remain.
    pub(super) fn consume(&mut self, count: u32) {
        let wanted = usize::try_from(count).unwrap_or(usize::MAX);
        if let Some(left) = self.remaining.checked_sub(wanted) {
            self.remaining = left;
        } else {
            self.remaining = 0;
            self.overflowed = true;
        }
    }

    /// Read and consume `count` (at most 56) bits.
    pub(super) fn read(&mut self, count: u32) -> u64 {
        let value = self.peek(count);
        self.consume(count);
        value
    }

    /// Whether any read so far went past the start of the stream.
    pub(super) fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Whether the stream was consumed to its last bit and not one further —
    /// the end condition of every well-formed Huffman and sequences stream.
    pub(super) fn finished_exactly(&self) -> bool {
        self.remaining == 0 && !self.overflowed
    }
}
