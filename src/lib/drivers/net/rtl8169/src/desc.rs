//! The 8168's descriptors: sixteen bytes each, the same shape both ways.
//!
//! | word | transmit                           | receive                        |
//! |------|------------------------------------|--------------------------------|
//! | 0    | flags, then the length in 15:0     | flags, then the size in 13:0   |
//! | 1    | VLAN and offload requests (none)   | VLAN tag, written back         |
//! | 2, 3 | the buffer's device address        | the buffer's device address    |
//!
//! Word 0's top four bits are the same in both: [`OWN`] gives the
//! descriptor to the chip, [`RING_END`] marks the ring's last one (the chip
//! goes back to the first after it), and [`FIRST`] and [`LAST`] bound a
//! frame. Word 0 is written last, after a barrier, since setting [`OWN`]
//! hands the other three over.

/// Bytes in one descriptor.
pub const DESCRIPTOR_BYTES: usize = 16;

/// The chip owns the descriptor.
pub const OWN: u32 = 1 << 31;
/// The ring's last descriptor.
pub const RING_END: u32 = 1 << 30;
/// The first descriptor of a frame.
pub const FIRST: u32 = 1 << 29;
/// The last descriptor of a frame.
pub const LAST: u32 = 1 << 28;

/// Receive: the frame has an error -- the summary of the bits below it.
pub const RX_ERROR: u32 = 1 << 21;
/// Receive: where the written-back length is, check sequence included.
pub const RX_LENGTH: u32 = 0x3FFF;
/// The check sequence's bytes, which the length counts.
pub const FCS_BYTES: u32 = 4;
/// The shortest frame a written-back length may describe: an Ethernet
/// header with its check sequence.
pub const MIN_FRAME: u32 = 14 + FCS_BYTES;

/// Transmit: the longest frame one descriptor carries.
pub const TX_LENGTH: u32 = 0xFFFF;

/// What a transmit descriptor holds for one frame of `len` bytes at
/// `address`: word 0 last, as [`DescriptorWords`] orders them.
#[must_use]
pub const fn transmit(address: u64, len: u32, end: bool) -> DescriptorWords {
    let mut word0 = OWN | FIRST | LAST | (len & TX_LENGTH);
    if end {
        word0 |= RING_END;
    }
    DescriptorWords {
        word0,
        word1: 0,
        address,
    }
}

/// What a receive descriptor holds lending a buffer of `bytes` at
/// `address` to the chip.
#[must_use]
pub const fn receive(address: u64, bytes: u32, end: bool) -> DescriptorWords {
    let mut word0 = OWN | (bytes & RX_LENGTH);
    if end {
        word0 |= RING_END;
    }
    DescriptorWords {
        word0,
        word1: 0,
        address,
    }
}

/// A descriptor's contents, to be written words 1 to 3 first and word 0
/// after a barrier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DescriptorWords {
    /// Flags and length; written last.
    pub word0: u32,
    /// Word 1.
    pub word1: u32,
    /// Words 2 and 3, little-endian.
    pub address: u64,
}

/// What a receive descriptor the chip gave back says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Received {
    /// A whole, good frame of this many bytes, check sequence removed.
    Frame(u32),
    /// An errored frame, a piece of a longer one, or a length no buffer
    /// of `bytes` holds: the buffer is lent again unread.
    Dropped,
}

/// Read a written-back receive descriptor's word 0, for a buffer of
/// `bytes`. A length is believed only inside the buffer: the chip writes
/// the buffer and the descriptor, and a descriptor that says more than the
/// buffer holds would have the driver copy what is past it.
#[must_use]
pub const fn received(word0: u32, bytes: u32) -> Received {
    if word0 & RX_ERROR != 0 || word0 & (FIRST | LAST) != (FIRST | LAST) {
        return Received::Dropped;
    }
    let len = word0 & RX_LENGTH;
    if len < MIN_FRAME || len > bytes {
        return Received::Dropped;
    }
    Received::Frame(len - FCS_BYTES)
}
