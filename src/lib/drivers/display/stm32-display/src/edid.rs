//! What a monitor says about itself in its EDID: whether the blocks are
//! whole, its name, whether it speaks HDMI rather than only DVI, the range
//! of timings it accepts, and every mode it offers.
//!
//! A monitor names its modes five ways, and a driver that wants the largest
//! one it can make has to read all of them (VESA E-EDID 1.4 and CTA-861-I;
//! Linux's `drm_edid.c` reads the same five):
//!
//! * **detailed timing descriptors**, spelled out in full: up to four in the
//!   base block, the first the preferred mode, and more in a CTA-861
//!   extension;
//! * **established timings**, a bit each for seventeen old VGA-era modes;
//! * **standard timings**, two bytes each naming a size and a refresh rate
//!   whose timing is VESA DMT's, eight in the base block and six more in a
//!   `0xFA` descriptor;
//! * **short video descriptors** in a CTA-861 extension's video data block,
//!   a byte each naming a CTA-861 video format by its code (VIC);
//! * and the **display range limits** descriptor, which offers no mode but
//!   bounds every timing the monitor locks to, and -- with the base block's
//!   continuous-frequency bit -- says it takes any timing inside them.
//!
//! [`offers`] reads the first four into an [`Offers`] list, in the order a
//! reader of the log expects them (the preferred mode first); [`range_limits`]
//! and [`continuous`] read the fifth. What the driver can actually run is
//! [`crate::choice`]'s question.

use crate::mode::Mode;
use crate::timings;

/// Bytes of one EDID block.
pub const BLOCK_BYTES: usize = 128;

/// The eight bytes every base block starts with.
pub const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];

/// A CEA-861 extension block's tag.
pub const CEA_EXTENSION: u8 = 0x02;

/// The IEEE OUI of HDMI Licensing, which an HDMI sink's vendor-specific data
/// block carries (00-0C-03, least significant byte first).
pub const HDMI_OUI: [u8; 3] = [0x03, 0x0C, 0x00];

/// The most offered modes an [`Offers`] keeps: four detailed timings, the
/// seventeen established ones, fourteen standard ones and a video data
/// block's thirty-one codes come to 66 in a base block and one extension,
/// and a real monitor lists fewer than half that.
pub const MAX_OFFERS: usize = 64;

/// Whether `block` sums to zero, as every EDID block must.
#[must_use]
pub fn checksum_ok(block: &[u8; BLOCK_BYTES]) -> bool {
    block.iter().fold(0_u8, |sum, byte| sum.wrapping_add(*byte)) == 0
}

/// Whether `block` is a base block: the header and the checksum.
#[must_use]
pub fn is_base(block: &[u8; BLOCK_BYTES]) -> bool {
    block.starts_with(&HEADER) && checksum_ok(block)
}

/// How many extension blocks follow the base block.
#[must_use]
pub fn extensions(block: &[u8; BLOCK_BYTES]) -> u8 {
    block.get(126).copied().unwrap_or(0)
}

/// The four 18-byte descriptors of a base block.
fn descriptors(block: &[u8; BLOCK_BYTES]) -> impl Iterator<Item = [u8; 18]> + '_ {
    (0..4).filter_map(move |index| {
        let start = 54 + index * 18;
        block.get(start..start + 18)?.try_into().ok()
    })
}

/// The display descriptors (a zero clock) of a base block with tag `tag`.
fn display_descriptors(block: &[u8; BLOCK_BYTES], tag: u8) -> impl Iterator<Item = [u8; 18]> + '_ {
    descriptors(block).filter(move |d| d.starts_with(&[0, 0, 0, tag]))
}

/// The preferred timing: the first descriptor, when it is a timing this
/// module can read.
#[must_use]
pub fn preferred(block: &[u8; BLOCK_BYTES]) -> Option<Mode> {
    Mode::from_detailed_timing(&descriptors(block).next()?)
}

/// The monitor's name from its name descriptor (tag 0xFC), up to thirteen
/// bytes, ended at the first newline, as ASCII.
#[must_use]
pub fn name(block: &[u8; BLOCK_BYTES]) -> Option<([u8; 13], usize)> {
    let descriptor = display_descriptors(block, 0xFC).next()?;
    let text: [u8; 13] = descriptor.get(5..18)?.try_into().ok()?;
    let len = text
        .iter()
        .position(|&byte| byte == b'\n' || byte == 0)
        .unwrap_or(text.len());
    Some((text, len))
}

/// The data blocks of a CEA-861 extension, as `(tag, payload)`: from byte 4
/// to the offset of the first detailed timing. Nothing for a block that is
/// not a whole CEA-861 extension.
fn data_blocks(extension: &[u8; BLOCK_BYTES]) -> impl Iterator<Item = (u8, &[u8])> + '_ {
    let [tag, _, end, ..] = *extension;
    let whole = tag == CEA_EXTENSION && checksum_ok(extension);
    let end = if whole {
        usize::from(end).min(BLOCK_BYTES - 1)
    } else {
        0
    };
    let mut at = 4;
    core::iter::from_fn(move || {
        if at >= end {
            return None;
        }
        let header = *extension.get(at)?;
        let (kind, len) = (header >> 5, usize::from(header & 0x1F));
        let payload = extension.get(at + 1..(at + 1 + len).min(end))?;
        at += 1 + len;
        Some((kind, payload))
    })
}

/// Whether a CEA-861 extension block says the sink is HDMI: a vendor-specific
/// data block (tag 3) carrying HDMI Licensing's OUI.
#[must_use]
pub fn is_hdmi(extension: &[u8; BLOCK_BYTES]) -> bool {
    data_blocks(extension)
        .any(|(kind, payload)| kind == 3 && payload.get(..3) == Some(&HDMI_OUI[..]))
}

/// The display range limits descriptor: the timings a monitor locks to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RangeLimits {
    /// Lowest and highest vertical rate, in Hz.
    pub vertical_hz: (u32, u32),
    /// Lowest and highest horizontal rate, in kHz.
    pub horizontal_khz: (u32, u32),
    /// Highest pixel clock, in kHz.
    pub max_clock_khz: u32,
}

/// The base block's display range limits (tag 0xFD), with EDID 1.4's
/// offsets of 255 applied, or `None` for a monitor that gives none.
#[must_use]
pub fn range_limits(block: &[u8; BLOCK_BYTES]) -> Option<RangeLimits> {
    let d = display_descriptors(block, 0xFD).next()?;
    let [_, _, _, _, offsets, v_min, v_max, h_min, h_max, clock, ..] = d;
    let plus = |value: u8, offset: bool| u32::from(value) + if offset { 255 } else { 0 };
    // Bits 1:0 say 10 for the maximum vertical rate's offset and 11 for
    // both; bits 3:2 the same for the horizontal rates.
    let limits = RangeLimits {
        vertical_hz: (
            plus(v_min, offsets & 0x3 == 0x3),
            plus(v_max, offsets & 0x2 != 0),
        ),
        horizontal_khz: (
            plus(h_min, offsets & 0xC == 0xC),
            plus(h_max, offsets & 0x8 != 0),
        ),
        max_clock_khz: u32::from(clock) * 10_000,
    };
    let sane = limits.vertical_hz.0 > 0
        && limits.vertical_hz.0 <= limits.vertical_hz.1
        && limits.horizontal_khz.0 > 0
        && limits.horizontal_khz.0 <= limits.horizontal_khz.1
        && limits.max_clock_khz > 0;
    sane.then_some(limits)
}

/// Whether the monitor takes any timing inside its range limits, not only
/// the ones it lists: the base block's feature byte, bit 0 -- "continuous
/// frequency" in EDID 1.4, "default GTF supported" in 1.3, which says the
/// same of the timings GTF makes.
#[must_use]
pub fn continuous(block: &[u8; BLOCK_BYTES]) -> bool {
    block.get(24).is_some_and(|features| features & 1 != 0)
}

/// Where a monitor offered a mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// Detailed timing descriptor `index` of block `block` (0 the base).
    Detailed {
        /// The block.
        block: u8,
        /// Its place among the block's timings, from 0.
        index: u8,
    },
    /// Established timing bit `0..17`: bytes 0x23 and 0x24 low bit first,
    /// then bit 7 of 0x25.
    Established(u8),
    /// A standard timing, its two bytes first byte high.
    Standard(u16),
    /// A CTA-861 short video descriptor.
    Vic {
        /// The code.
        vic: u8,
        /// Whether the sink calls it its native format.
        native: bool,
    },
}

/// Why an offered mode is not one this driver can run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unusable {
    /// Interlaced: the LTDC scans out whole frames only.
    Interlaced,
    /// A detailed timing whose sync is analog or composite, or whose numbers
    /// do not hang together.
    Timing,
    /// A code the tables here do not keep: a pixel-repeated or interlaced
    /// VIC, one past 127, or a standard timing DMT has no timing for.
    Unknown,
}

/// One offered mode, and the timing it comes to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Offer {
    /// Where the monitor said it.
    pub source: Source,
    /// The timing, or why there is none to run.
    pub mode: Result<Mode, Unusable>,
}

/// Every mode a monitor offers, in the order its EDID gives them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Offers {
    list: [Offer; MAX_OFFERS],
    len: usize,
    /// Offers past [`MAX_OFFERS`], counted and dropped.
    dropped: usize,
}

impl Offers {
    const EMPTY: Offer = Offer {
        source: Source::Established(0),
        mode: Err(Unusable::Unknown),
    };

    fn new() -> Self {
        Offers {
            list: [Self::EMPTY; MAX_OFFERS],
            len: 0,
            dropped: 0,
        }
    }

    fn push(&mut self, source: Source, mode: Result<Mode, Unusable>) {
        match self.list.get_mut(self.len) {
            Some(slot) => {
                *slot = Offer { source, mode };
                self.len += 1;
            }
            None => self.dropped += 1,
        }
    }

    /// The offers.
    #[must_use]
    pub fn as_slice(&self) -> &[Offer] {
        self.list.get(..self.len).unwrap_or(&[])
    }

    /// Offers dropped past [`MAX_OFFERS`].
    #[must_use]
    pub const fn dropped(&self) -> usize {
        self.dropped
    }
}

/// What detailed timing descriptor `d` describes: `None` for a display
/// descriptor, which is not a timing at all.
fn detailed(d: &[u8; 18]) -> Option<Result<Mode, Unusable>> {
    let [c0, c1, .., flags] = *d;
    if c0 == 0 && c1 == 0 {
        return None;
    }
    if flags & 0x80 != 0 {
        return Some(Err(Unusable::Interlaced));
    }
    let Some(mut mode) = Mode::from_detailed_timing(d) else {
        return Some(Err(Unusable::Timing));
    };
    // A detailed timing that is a CTA-861 format is sent as one, so an HDMI
    // sink's AVI infoframe names it.
    mode.vic = timings::vic_of(&mode).map_or(0, |format| format.vic);
    Some(Ok(mode))
}

/// A short video descriptor's byte as `(vic, native)`: codes 1 to 64 plain
/// or, from CTA-861-F on, 129 to 192 with the native bit; 65 to 127 and 193
/// to 253 plain, with no native bit to spare; 0, 128, 254 and 255 reserved.
fn short_video(byte: u8) -> Option<(u8, bool)> {
    match byte {
        1..=127 | 193..=253 => Some((byte, false)),
        129..=192 => Some((byte & 0x7F, true)),
        _ => None,
    }
}

/// The mode a VIC names.
fn vic_mode(vic: u8) -> Result<Mode, Unusable> {
    timings::vic(vic)
        .map(timings::Vic::mode)
        .ok_or(Unusable::Unknown)
}

/// The mode a standard timing's two bytes name, or `None` for the unused
/// patterns (`01 01`, and the `00 00` and `20 20` some monitors write).
fn standard(first: u8, second: u8) -> Option<Result<Mode, Unusable>> {
    if matches!((first, second), (0x01, 0x01) | (0x00, 0x00) | (0x20, 0x20)) || first == 0 {
        return None;
    }
    let code = u16::from_be_bytes([first, second]);
    Some(
        timings::standard(code)
            .map(timings::Standard::mode)
            .ok_or(Unusable::Unknown),
    )
}

/// The size and refresh a standard timing's code names, whether or not DMT
/// has a timing for it (EDID 1.3 and on, where aspect 0 is 16:10).
#[must_use]
pub fn standard_size(code: u16) -> (u32, u32, u32) {
    let [first, second] = code.to_be_bytes();
    let width = (u32::from(first) + 31) * 8;
    let height = match second >> 6 {
        0 => width * 10 / 16,
        1 => width * 3 / 4,
        2 => width * 4 / 5,
        _ => width * 9 / 16,
    };
    (width, height, u32::from(second & 0x3F) + 60)
}

/// Every mode `base` and the blocks after it offer, the base block's first:
/// its detailed timings (the preferred mode first), its established
/// timings, its standard timings, then each CTA-861 extension's short video
/// descriptors and detailed timings. Other extensions are passed over.
///
/// `base` must be a base block ([`is_base`]); the caller reads only the
/// extensions [`extensions`] says there are.
#[must_use]
pub fn offers(base: &[u8; BLOCK_BYTES], following: &[[u8; BLOCK_BYTES]]) -> Offers {
    let mut offers = Offers::new();
    for (index, mode) in descriptors(base).filter_map(|d| detailed(&d)).enumerate() {
        let index = u8::try_from(index).unwrap_or(u8::MAX);
        offers.push(Source::Detailed { block: 0, index }, mode);
    }
    let [a, b, c] = [35, 36, 37].map(|at| base.get(at).copied().unwrap_or(0));
    let bits = u32::from(a) | (u32::from(b) << 8) | (u32::from(c & 0x80) << 9);
    for bit in 0..timings::ESTABLISHED_BITS {
        if bits & (1 << bit) != 0 {
            let mode = timings::established(bit).ok_or(Unusable::Interlaced);
            offers.push(Source::Established(bit), mode);
        }
    }
    let codes = base.get(38..54).unwrap_or(&[]).as_chunks::<2>().0;
    let more: [u8; 12] = display_descriptors(base, 0xFA)
        .next()
        .and_then(|d| d.get(5..17)?.try_into().ok())
        .unwrap_or([0x01; 12]);
    for &[first, second] in codes.iter().chain(more.as_chunks::<2>().0) {
        if let Some(mode) = standard(first, second) {
            offers.push(Source::Standard(u16::from_be_bytes([first, second])), mode);
        }
    }
    for (number, extension) in following.iter().enumerate() {
        let block = u8::try_from(number + 1).unwrap_or(u8::MAX);
        for (kind, payload) in data_blocks(extension) {
            // Tag 2: the video data block.
            if kind != 2 {
                continue;
            }
            for (vic, native) in payload.iter().filter_map(|&byte| short_video(byte)) {
                offers.push(Source::Vic { vic, native }, vic_mode(vic));
            }
        }
        let [tag, _, start, ..] = *extension;
        if tag != CEA_EXTENSION || !checksum_ok(extension) || start < 4 {
            continue;
        }
        let mut at = usize::from(start);
        let mut index = 0_u8;
        // Timings run to the checksum byte, the last, at most.
        while let Some(d) = extension
            .get(at..at + 18)
            .filter(|_| at + 18 < BLOCK_BYTES)
            .and_then(|d| <[u8; 18]>::try_from(d).ok())
        {
            let Some(mode) = detailed(&d) else {
                break;
            };
            offers.push(Source::Detailed { block, index }, mode);
            index = index.saturating_add(1);
            at += 18;
        }
    }
    offers
}
