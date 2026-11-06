//! TCP headers and options, RFC 9293 and RFC 7323.
//!
//! This is the header, not the protocol: the state machine, its timers and its
//! congestion arithmetic are a crate of their own. A parse checks the data
//! offset and the checksum over the pseudo-header, walks the options, keeps the
//! ones a connection negotiates — maximum segment size, window scale, SACK
//! permission and blocks, timestamps — refuses those whose length is wrong for
//! their type, and skips any other by its length. An emit writes the options it
//! is given, padded with end-of-list bytes to a whole word.

use crate::Error;
use crate::checksum::Pseudo;
use crate::wire;

/// Bytes in a header with no options.
pub const MIN_HEADER_LEN: usize = 20;

/// Bytes in a header with the most options its data offset allows.
pub const MAX_HEADER_LEN: usize = 60;

/// The IP protocol number, and IPv6 next header value, of TCP.
pub const PROTOCOL: u8 = 6;

/// The most SACK blocks forty bytes of options can carry.
pub const MAX_SACK_BLOCKS: usize = 4;

/// Option types.
mod option_kind {
    /// End of the option list.
    pub(super) const END: u8 = 0;
    /// No operation.
    pub(super) const NOP: u8 = 1;
    /// Maximum segment size.
    pub(super) const MSS: u8 = 2;
    /// Window scale.
    pub(super) const WINDOW_SCALE: u8 = 3;
    /// SACK permitted.
    pub(super) const SACK_PERMITTED: u8 = 4;
    /// SACK blocks.
    pub(super) const SACK: u8 = 5;
    /// Timestamps.
    pub(super) const TIMESTAMPS: u8 = 8;
}

/// The bits the flags field holds: the eight flags and accurate ECN's AE.
const FLAG_BITS: u16 = 0x01FF;

/// A header's control flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags(pub u16);

impl Flags {
    /// No more data from the sender.
    pub const FIN: Flags = Flags(0x001);
    /// Synchronize sequence numbers.
    pub const SYN: Flags = Flags(0x002);
    /// Reset the connection.
    pub const RST: Flags = Flags(0x004);
    /// Push.
    pub const PSH: Flags = Flags(0x008);
    /// The acknowledgment field is significant.
    pub const ACK: Flags = Flags(0x010);
    /// The urgent pointer is significant.
    pub const URG: Flags = Flags(0x020);
    /// ECN echo.
    pub const ECE: Flags = Flags(0x040);
    /// Congestion window reduced.
    pub const CWR: Flags = Flags(0x080);

    /// Whether every flag in `other` is set.
    #[must_use]
    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    /// The flags of both.
    #[must_use]
    pub const fn union(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }
}

/// One SACK block: the sequence numbers of its first byte and of the byte after
/// its last.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SackBlock {
    /// The first sequence number in the block.
    pub left: u32,
    /// The sequence number after the block.
    pub right: u32,
}

/// The options a connection negotiates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Options {
    /// Maximum segment size.
    pub mss: Option<u16>,
    /// Window scale shift.
    pub window_scale: Option<u8>,
    /// SACK is permitted.
    pub sack_permitted: bool,
    /// SACK blocks; the first [`Options::sack_blocks`] are in use.
    pub sack: [SackBlock; MAX_SACK_BLOCKS],
    /// How many SACK blocks are in use.
    pub sack_blocks: usize,
    /// The timestamp value and echo reply.
    pub timestamp: Option<(u32, u32)>,
}

impl Options {
    /// Bytes these options take written out, before padding to a word.
    #[must_use]
    pub fn len(&self) -> usize {
        let blocks = self.sack().len();
        self.mss.map_or(0, |_| 4)
            + self.window_scale.map_or(0, |_| 3)
            + if self.sack_permitted { 2 } else { 0 }
            + if blocks > 0 { 2 + 8 * blocks } else { 0 }
            + self.timestamp.map_or(0, |_| 10)
    }

    /// Whether there are no options.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The SACK blocks in use.
    #[must_use]
    pub fn sack(&self) -> &[SackBlock] {
        self.sack.get(..self.sack_blocks).unwrap_or(&[])
    }
}

/// A TCP header, less the data offset and checksum a parse checks and an emit
/// computes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    /// Source port.
    pub source_port: u16,
    /// Destination port.
    pub destination_port: u16,
    /// Sequence number.
    pub sequence: u32,
    /// Acknowledgment number.
    pub acknowledgment: u32,
    /// Control flags.
    pub flags: Flags,
    /// Window, before scaling.
    pub window: u16,
    /// Urgent pointer.
    pub urgent_pointer: u16,
    /// Options.
    pub options: Options,
}

/// A parsed segment.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Segment<'a> {
    /// The header.
    pub header: Header,
    /// The data after the options.
    pub payload: &'a [u8],
}

impl Header {
    /// Read the segment that is all of `bytes`, which arrived with the
    /// addresses in `pseudo`.
    pub fn parse(bytes: &[u8], pseudo: Pseudo) -> Result<Segment<'_>, Error> {
        let offset = wire::byte(bytes, 12).ok_or(Error::Truncated)?;
        let header_len = usize::from(offset >> 4) * 4;
        if header_len < MIN_HEADER_LEN {
            return Err(Error::Malformed("a data offset below five words"));
        }
        let option_bytes = bytes
            .get(MIN_HEADER_LEN..header_len)
            .ok_or(Error::Truncated)?;
        let mut sum = pseudo
            .sum(PROTOCOL, bytes.len())
            .ok_or(Error::Malformed("a segment the pseudo-header cannot hold"))?;
        sum.add_bytes(bytes);
        if sum.finish() != 0 {
            return Err(Error::BadChecksum);
        }
        let header = Header {
            source_port: wire::be16(bytes, 0).ok_or(Error::Truncated)?,
            destination_port: wire::be16(bytes, 2).ok_or(Error::Truncated)?,
            sequence: wire::be32(bytes, 4).ok_or(Error::Truncated)?,
            acknowledgment: wire::be32(bytes, 8).ok_or(Error::Truncated)?,
            flags: Flags(wire::be16(bytes, 12).ok_or(Error::Truncated)? & FLAG_BITS),
            window: wire::be16(bytes, 14).ok_or(Error::Truncated)?,
            urgent_pointer: wire::be16(bytes, 18).ok_or(Error::Truncated)?,
            options: parse_options(option_bytes)?,
        };
        Ok(Segment {
            header,
            payload: bytes.get(header_len..).ok_or(Error::Truncated)?,
        })
    }

    /// The sequence space a segment of this header and `payload_len` bytes
    /// occupies: its data, and one each for SYN and FIN.
    #[must_use]
    pub const fn sequence_len(&self, payload_len: usize) -> usize {
        let syn = if self.flags.contains(Flags::SYN) {
            1
        } else {
            0
        };
        let fin = if self.flags.contains(Flags::FIN) {
            1
        } else {
            0
        };
        payload_len + syn + fin
    }

    /// Write the segment carrying `payload`, to be sent with the addresses in
    /// `pseudo`, at the start of `out`, returning its length.
    pub fn emit(&self, payload: &[u8], pseudo: Pseudo, out: &mut [u8]) -> Result<usize, Error> {
        if self.options.sack_blocks > MAX_SACK_BLOCKS || self.flags.0 & !FLAG_BITS != 0 {
            return Err(Error::Malformed(
                "more than four SACK blocks or an unknown flag",
            ));
        }
        let options_len = self.options.len().next_multiple_of(4);
        if options_len > MAX_HEADER_LEN - MIN_HEADER_LEN {
            return Err(Error::Malformed("options over 40 bytes"));
        }
        let header_len = MIN_HEADER_LEN + options_len;
        let len = header_len
            .checked_add(payload.len())
            .ok_or(Error::NoSpace)?;
        let segment = out.get_mut(..len).ok_or(Error::NoSpace)?;
        let words = u16::try_from(header_len / 4).map_err(|_| Error::NoSpace)?;
        let fields: [(usize, &[u8]); 8] = [
            (0, &self.source_port.to_be_bytes()),
            (2, &self.destination_port.to_be_bytes()),
            (4, &self.sequence.to_be_bytes()),
            (8, &self.acknowledgment.to_be_bytes()),
            (12, &((words << 12) | self.flags.0).to_be_bytes()),
            (14, &self.window.to_be_bytes()),
            (16, &[0, 0]),
            (18, &self.urgent_pointer.to_be_bytes()),
        ];
        for (at, field) in fields {
            wire::put(segment, at, field).ok_or(Error::NoSpace)?;
        }
        let area = segment
            .get_mut(MIN_HEADER_LEN..header_len)
            .ok_or(Error::NoSpace)?;
        area.fill(option_kind::END);
        write_options(&self.options, area)?;
        wire::put(segment, header_len, payload).ok_or(Error::NoSpace)?;
        let mut sum = pseudo
            .sum(PROTOCOL, len)
            .ok_or(Error::Malformed("a segment the pseudo-header cannot hold"))?;
        sum.add_bytes(segment);
        wire::put(segment, 16, &sum.finish().to_be_bytes()).ok_or(Error::NoSpace)?;
        Ok(len)
    }
}

/// Walk the option bytes, keeping the options a connection negotiates.
fn parse_options(bytes: &[u8]) -> Result<Options, Error> {
    let mut options = Options::default();
    let mut rest = bytes;
    while let Some((&kind, tail)) = rest.split_first() {
        if kind == option_kind::END {
            break;
        }
        if kind == option_kind::NOP {
            rest = tail;
            continue;
        }
        let len = usize::from(
            *tail
                .first()
                .ok_or(Error::Malformed("an option without its length"))?,
        );
        if len < 2 {
            return Err(Error::Malformed("an option length below two"));
        }
        let option = rest
            .get(..len)
            .ok_or(Error::Malformed("an option running past the header"))?;
        read_option(&mut options, kind, option.get(2..).unwrap_or(&[]))?;
        rest = rest.get(len..).unwrap_or(&[]);
    }
    Ok(options)
}

/// Keep one option's value, if it is one a connection negotiates.
fn read_option(options: &mut Options, kind: u8, data: &[u8]) -> Result<(), Error> {
    match (kind, data.len()) {
        (option_kind::MSS, 2) => options.mss = wire::be16(data, 0),
        (option_kind::WINDOW_SCALE, 1) => options.window_scale = wire::byte(data, 0),
        (option_kind::SACK_PERMITTED, 0) => options.sack_permitted = true,
        (option_kind::TIMESTAMPS, 8) => {
            options.timestamp = wire::be32(data, 0).zip(wire::be32(data, 4));
        }
        (option_kind::SACK, n) if n > 0 && n.is_multiple_of(8) && n / 8 <= MAX_SACK_BLOCKS => {
            read_sack(options, data);
        }
        (
            option_kind::MSS
            | option_kind::WINDOW_SCALE
            | option_kind::SACK_PERMITTED
            | option_kind::TIMESTAMPS
            | option_kind::SACK,
            _,
        ) => return Err(Error::Malformed("a TCP option of the wrong length")),
        _ => {}
    }
    Ok(())
}

/// Keep the SACK blocks in `data`, eight bytes each.
fn read_sack(options: &mut Options, data: &[u8]) {
    options.sack_blocks = 0;
    for (slot, chunk) in options.sack.iter_mut().zip(data.as_chunks::<8>().0) {
        *slot = SackBlock {
            left: wire::be32(chunk, 0).unwrap_or(0),
            right: wire::be32(chunk, 4).unwrap_or(0),
        };
        options.sack_blocks += 1;
    }
}

/// Write `options` into `out`, which is already filled with end-of-list bytes.
fn write_options(options: &Options, out: &mut [u8]) -> Result<(), Error> {
    let mut at = 0;
    let mut push = |bytes: &[u8]| -> Result<(), Error> {
        wire::put(out, at, bytes).ok_or(Error::NoSpace)?;
        at += bytes.len();
        Ok(())
    };
    if let Some(mss) = options.mss {
        let [high, low] = mss.to_be_bytes();
        push(&[option_kind::MSS, 4, high, low])?;
    }
    if let Some(shift) = options.window_scale {
        push(&[option_kind::WINDOW_SCALE, 3, shift])?;
    }
    if options.sack_permitted {
        push(&[option_kind::SACK_PERMITTED, 2])?;
    }
    if let Some((value, echo)) = options.timestamp {
        push(&[option_kind::TIMESTAMPS, 10])?;
        push(&value.to_be_bytes())?;
        push(&echo.to_be_bytes())?;
    }
    let blocks = options.sack();
    if !blocks.is_empty() {
        let len = u8::try_from(2 + 8 * blocks.len()).map_err(|_| Error::NoSpace)?;
        push(&[option_kind::SACK, len])?;
        for block in blocks {
            push(&block.left.to_be_bytes())?;
            push(&block.right.to_be_bytes())?;
        }
    }
    Ok(())
}
