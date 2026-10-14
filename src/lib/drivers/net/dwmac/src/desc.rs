//! The core's normal descriptors: sixteen bytes, four little-endian words,
//! in a "read" form the driver writes and a "write-back" form the DMA
//! leaves in the same place when it is done.

/// Bytes in one descriptor.
pub const DESCRIPTOR_BYTES: usize = 16;

/// The DMA owns the descriptor: bit 31 of the fourth word, in both forms
/// and both directions.
pub const OWN: u32 = 1 << 31;

/// Transmit, read form, word 2: interrupt on completion.
pub const TX_IOC: u32 = 1 << 31;
/// Transmit, word 3: the frame's first descriptor.
pub const TX_FD: u32 = 1 << 29;
/// Transmit, word 3: the frame's last descriptor.
pub const TX_LD: u32 = 1 << 28;
/// Transmit, write-back, word 3: error summary.
pub const TX_ES: u32 = 1 << 15;
/// The length fields: buffer 1's in word 2, the frame's in word 3.
pub const TX_LENGTH: u32 = 0x3FFF;

/// Receive, read form, word 3: interrupt on completion.
pub const RX_IOC: u32 = 1 << 30;
/// Receive, read form, word 3: buffer 1's address is valid.
pub const RX_BUF1V: u32 = 1 << 24;
/// Receive, write-back, word 3: a context descriptor, not a frame.
pub const RX_CTXT: u32 = 1 << 30;
/// Receive, write-back, word 3: the frame's first descriptor.
pub const RX_FD: u32 = 1 << 29;
/// Receive, write-back, word 3: the frame's last descriptor.
pub const RX_LD: u32 = 1 << 28;
/// Receive, write-back, word 3: error summary.
pub const RX_ES: u32 = 1 << 15;
/// Receive, write-back, word 3: the frame's length, bits 14:0.
pub const RX_PL: u32 = 0x7FFF;

/// The four words of a transmit descriptor handing `len` bytes at `address`
/// to the DMA as one whole frame, with the CRC and padding inserted by the
/// core. Word 3, which carries [`OWN`], is the one to write last.
#[must_use]
pub const fn transmit(address: u64, len: u32) -> [u32; 4] {
    let len = len & TX_LENGTH;
    [
        address as u32,
        (address >> 32) as u32,
        TX_IOC | len,
        OWN | TX_FD | TX_LD | len,
    ]
}

/// The four words of a receive descriptor lending the DMA the buffer at
/// `address`. Its size is the channel's, set once. Word 3 is the one to
/// write last.
#[must_use]
pub const fn receive(address: u64) -> [u32; 4] {
    [
        address as u32,
        (address >> 32) as u32,
        0,
        OWN | RX_IOC | RX_BUF1V,
    ]
}

/// A receive descriptor the DMA gave back, read from its fourth word.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Received {
    /// A whole frame of this many bytes, without its check sequence, in the
    /// descriptor's own buffer.
    Frame(u32),
    /// Anything else: an error, a frame that did not fit one buffer, a part
    /// of one, a context descriptor, or a length no buffer holds. Dropped,
    /// and the buffer lent again.
    Dropped,
}

/// What the fourth word of a receive descriptor the DMA no longer owns
/// says, for buffers of `buffer_bytes`.
///
/// The length is checked against the buffer and against the smallest frame
/// there is -- fourteen bytes of Ethernet header -- because it is the
/// device's word and decides how many bytes are copied out.
#[must_use]
pub const fn received(word3: u32, buffer_bytes: u32) -> Received {
    let whole = RX_FD | RX_LD;
    if word3 & RX_CTXT != 0 || word3 & whole != whole || word3 & RX_ES != 0 {
        return Received::Dropped;
    }
    let len = word3 & RX_PL;
    if len < 14 || len > buffer_bytes {
        return Received::Dropped;
    }
    Received::Frame(len)
}
