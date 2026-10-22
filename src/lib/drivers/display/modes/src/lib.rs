//! A scanout's mode list, as a ring-3 display driver makes it
//! (`docs/DISPLAY.md` §9).
//!
//! A driver lists every mode it can show a display at in `MODE_LIST`, and is
//! told one of them by `SET_MODE`. What goes in the list is the driver's to
//! judge, since only it knows its limits; this module is what every driver
//! makes its list with, so that they agree on the rest:
//!
//! * [`ModeList`]: at most [`MAX_LISTED_MODES`], none twice, in Linux's
//!   order (`drm_mode_sort`), and cut into `MODE_LIST` pages.
//! * [`edid_modes`]: every mode an EDID names, timed as Linux times them
//!   (`drm_edid.c`): its detailed timings, established timings, standard
//!   timings, Established Timings III, CTA-861 video codes and detailed
//!   timings, and `DisplayID` type I timings.
//! * [`cvt`]: VESA's Coordinated Video Timings, as `drm_cvt_mode` makes
//!   them, for a standard timing no table names.
//! * [`add_virtual`]: what a virtual card, which shows any size, lists
//!   beside its display's modes.
//! * [`add_scaled`]: the smaller sizes a card shows by scaling a buffer to
//!   the one mode its display runs.
//!
//! [`scale`] is the arithmetic of showing a buffer smaller than the screen,
//! scaled, for a driver that cannot retime its display.
//!
//! Drivers' alone: the kernel never parses an EDID or makes a timing for a
//! mode list (the customer, 2026-10-22: "keep the kernel slim"); it carries
//! the list a driver sends (`ferrix_displayctl`) and passes a chosen entry
//! back. Nothing here allocates: a list is a fixed array, and the native
//! drivers have no allocator.

#![no_std]
#![forbid(unsafe_code)]

mod edid_tables;
pub mod scale;

#[cfg(test)]
mod tests;

use crate::edid_tables::{CTA, DMT, ESTABLISHED, ESTABLISHED3};
use ferrix_displayctl::message::{
    Listed, MAX_DIMENSION, MAX_LISTED_MODES, MAX_REFRESH_MHZ, MODES_A_PAGE, Message, ModePage,
    Timing,
};

/// A scanout's modes, as a driver collects them before it sends them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ModeList {
    count: usize,
    list: [Listed; MAX_LISTED_MODES],
}

impl Default for ModeList {
    fn default() -> Self {
        Self::new()
    }
}

impl ModeList {
    /// No modes.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            count: 0,
            list: [Listed {
                timing: Timing {
                    clock_khz: 0,
                    hdisplay: 0,
                    hsync_start: 0,
                    hsync_end: 0,
                    htotal: 0,
                    vdisplay: 0,
                    vsync_start: 0,
                    vsync_end: 0,
                    vtotal: 0,
                    hsync_high: false,
                    vsync_high: false,
                },
                preferred: false,
            }; MAX_LISTED_MODES],
        }
    }

    /// The modes.
    #[must_use]
    pub fn as_slice(&self) -> &[Listed] {
        self.list.get(..self.count).unwrap_or(&self.list)
    }

    /// How many.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    /// Whether there are none.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Add `timing`, preferred or not. Not added, and `false`: a timing that
    /// is not a mode a display can have, one of a size and refresh already
    /// listed (within 5 mHz) (the first one listed stays, so a display's own timing stays
    /// before a made-up one), or a list that is full. A preferred one takes
    /// the mark from any other, so a list has one.
    pub fn add(&mut self, timing: Timing, preferred: bool) -> bool {
        let refresh = timing.refresh_mhz();
        if !timing.is_mode()
            || u32::from(timing.vdisplay) > MAX_DIMENSION
            || !(1..=u64::from(MAX_REFRESH_MHZ)).contains(&refresh)
        {
            return false;
        }
        // The same size within 5 millihertz is the same mode: 59.94 Hz and
        // 60 Hz, which a monitor lists both of, stay two.
        let same = |listed: &Listed| {
            (listed.timing.hdisplay, listed.timing.vdisplay) == (timing.hdisplay, timing.vdisplay)
                && listed.timing.refresh_mhz().abs_diff(refresh) <= 5
        };
        if let Some(at) = self.as_slice().iter().position(same) {
            if preferred {
                self.prefer(at);
            }
            return false;
        }
        let Some(slot) = self.list.get_mut(self.count) else {
            return false;
        };
        *slot = Listed {
            timing,
            preferred: false,
        };
        self.count += 1;
        if preferred {
            self.prefer(self.count - 1);
        }
        true
    }

    /// Mark entry `at` preferred, and no other.
    fn prefer(&mut self, at: usize) {
        for (index, listed) in self.list.iter_mut().take(self.count).enumerate() {
            listed.preferred = index == at;
        }
    }

    /// Whether one entry is preferred.
    #[must_use]
    pub fn has_preferred(&self) -> bool {
        self.as_slice().iter().any(|listed| listed.preferred)
    }

    /// Mark the first entry preferred if none is: a list always has one.
    pub fn ensure_preferred(&mut self) {
        if !self.has_preferred() && self.count > 0 {
            self.prefer(0);
        }
    }

    /// Sort as Linux's `drm_mode_sort` does: the preferred mode first, then
    /// by pixels, then by refresh, then by clock, each the larger first.
    pub fn sort(&mut self) {
        let key = |listed: &Listed| {
            let timing = &listed.timing;
            (
                listed.preferred,
                u32::from(timing.hdisplay) * u32::from(timing.vdisplay),
                timing.refresh_mhz(),
                timing.clock_khz,
            )
        };
        // Insertion sort: at most 64, and stable, so ties keep the order
        // they were found in.
        for index in 1..self.count {
            let mut at = index;
            while at > 0 {
                let (Some(before), Some(here)) = (self.list.get(at - 1), self.list.get(at)) else {
                    break;
                };
                if key(before) >= key(here) {
                    break;
                }
                self.list.swap(at - 1, at);
                at -= 1;
            }
        }
    }

    /// The `MODE_LIST` pages that send the list for `scanout`, in order;
    /// none for an empty list.
    pub fn pages(&self, scanout: u32) -> impl Iterator<Item = Message> + '_ {
        let total = u32::try_from(self.count).unwrap_or(0);
        self.as_slice()
            .chunks(MODES_A_PAGE)
            .enumerate()
            .map(move |(index, chunk)| {
                let mut entries = [Listed::default(); MODES_A_PAGE];
                for (into, from) in entries.iter_mut().zip(chunk) {
                    *into = *from;
                }
                Message::ModeList(ModePage {
                    scanout,
                    total,
                    first: u32::try_from(index * MODES_A_PAGE).unwrap_or(u32::MAX),
                    count: u32::try_from(chunk.len()).unwrap_or(0),
                    entries,
                })
            })
    }

    /// The listed timing a size and refresh name: of that size, the one
    /// whose refresh is nearest `refresh_mhz`.
    #[must_use]
    pub fn find(&self, width: u32, height: u32, refresh_mhz: u64) -> Option<Timing> {
        self.as_slice()
            .iter()
            .map(|listed| listed.timing)
            .filter(|timing| {
                (u32::from(timing.hdisplay), u32::from(timing.vdisplay)) == (width, height)
            })
            .min_by_key(|timing| timing.refresh_mhz().abs_diff(refresh_mhz))
    }
}

/// The sizes a virtual card lists beside its display's own, and a copying
/// card shows smaller than its display by scaling: the ones Linux's
/// `drm_add_modes_noedid` reaches most often (`docs/DISPLAY.md` §2.3).
pub const STANDARD_SIZES: [(u32, u32); 10] = [
    (3840, 2160),
    (2560, 1440),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1600, 900),
    (1280, 1024),
    (1280, 720),
    (1024, 768),
    (800, 600),
];

/// The refreshes a virtual card lists its preferred size at beside its
/// display's, in hertz (`docs/DISPLAY.md` §2.3).
pub const OFFERED_REFRESHES_HZ: [u32; 3] = [60, 120, 144];

/// A made-up timing for a size at `refresh_mhz`: sync pulses where a
/// monitor would put them and the pixel clock the refresh takes over those
/// totals, in whole kHz rounded down. The numbers the kernel's own listing
/// of a virtual card has always made (`drm.rs`'s `mode_at`), so that a
/// mode a virtio-gpu lists is the one it listed before.
#[must_use]
pub fn synthetic(width: u32, height: u32, refresh_mhz: u32) -> Timing {
    let clamp = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
    let mut timing = Timing {
        clock_khz: 0,
        hdisplay: clamp(width),
        hsync_start: clamp(width + 16),
        hsync_end: clamp(width + 48),
        htotal: clamp(width + 160),
        vdisplay: clamp(height),
        vsync_start: clamp(height + 3),
        vsync_end: clamp(height + 9),
        vtotal: clamp(height + 23),
        hsync_high: false,
        vsync_high: false,
    };
    let total = u64::from(timing.htotal) * u64::from(timing.vtotal);
    timing.clock_khz = u32::try_from(total * u64::from(refresh_mhz) / 1_000_000).unwrap_or(0);
    timing
}

/// What a virtual card -- a virtio-gpu, which shows any size it is handed
/// a scanout of -- lists beside its display's EDID: the preferred size at
/// the refresh its display reported (60 Hz when it reported none),
/// preferred where the EDID named none of that size, the same size at
/// [`OFFERED_REFRESHES_HZ`], then [`STANDARD_SIZES`] at 60.
pub fn add_virtual(list: &mut ModeList, width: u32, height: u32, refresh_mhz: u32) {
    let refresh = if refresh_mhz == 0 {
        60_000
    } else {
        refresh_mhz
    };
    let reported = synthetic(width, height, refresh);
    let preferred_named = list.as_slice().iter().any(|listed| {
        listed.preferred
            && (
                u32::from(listed.timing.hdisplay),
                u32::from(listed.timing.vdisplay),
            ) == (width, height)
    });
    let _ = list.add(reported, !preferred_named);
    for hertz in OFFERED_REFRESHES_HZ {
        let _ = list.add(synthetic(width, height, hertz * 1000), false);
    }
    for (wide, tall) in STANDARD_SIZES {
        let _ = list.add(synthetic(wide, tall, 60_000), false);
    }
}

/// The sizes a card that cannot retime its display offers below its own,
/// shown scaled: the common desktop sizes of 4:3, 5:4, 16:10 and 16:9 down
/// to VGA's, as Linux's `drm_add_modes_noedid` offers a panel the DMT
/// sizes that fit it.
pub const SCALED_SIZES: [(u32, u32); 16] = [
    (3840, 2160),
    (2560, 1600),
    (2560, 1440),
    (1920, 1200),
    (1920, 1080),
    (1680, 1050),
    (1600, 900),
    (1440, 900),
    (1366, 768),
    (1280, 1024),
    (1280, 800),
    (1280, 720),
    (1024, 768),
    (1024, 600),
    (800, 600),
    (640, 480),
];

/// What a card that cannot retime its display lists: the mode it runs,
/// preferred, and each of [`SCALED_SIZES`] that fits inside it, at the
/// same refresh, which it shows by scaling (`docs/DISPLAY.md` §9.2).
pub fn add_scaled(list: &mut ModeList, running: Timing) {
    let _ = list.add(running, true);
    let refresh = u32::try_from(running.refresh_mhz()).unwrap_or(60_000);
    for (wide, tall) in SCALED_SIZES {
        let inside = wide <= u32::from(running.hdisplay)
            && tall <= u32::from(running.vdisplay)
            && (wide, tall) != (u32::from(running.hdisplay), u32::from(running.vdisplay));
        if inside {
            let _ = list.add(synthetic(wide, tall, refresh), false);
        }
    }
}

/// VESA's Coordinated Video Timing for `width` by `height` at `hertz`,
/// with reduced blanking or not, as Linux's `drm_cvt_mode` makes it with no
/// margins and no interlacing -- its clock included, which Linux leaves
/// unrounded to 250 kHz. `None` for a size or rate it cannot time.
#[must_use]
pub fn cvt(width: u32, height: u32, hertz: u32, reduced: bool) -> Option<Timing> {
    const HV_FACTOR: u64 = 1000;
    const H_GRANULARITY: u32 = 8;
    const MIN_V_PORCH: u32 = 3;
    const MIN_V_BPORCH: u32 = 6;
    if width == 0 || height == 0 || hertz == 0 || hertz > 1000 {
        return None;
    }
    let field = u64::from(hertz);
    let hdisplay = width - width % H_GRANULARITY;
    let vdisplay = height;
    let vsync = if height.is_multiple_of(3) && height * 4 / 3 == width {
        4
    } else if height.is_multiple_of(9) && height * 16 / 9 == width {
        5
    } else if height.is_multiple_of(10) && height * 16 / 10 == width {
        6
    } else if (height.is_multiple_of(4) && height * 5 / 4 == width)
        || (height.is_multiple_of(9) && height * 15 / 9 == width)
    {
        7
    } else {
        10
    };
    let (htotal, hsync_start, hsync_end, vtotal, vsync_start, hperiod, hsync_high);
    if reduced {
        const RB_MIN_VBLANK: u64 = 460;
        const RB_H_SYNC: u32 = 32;
        const RB_H_BLANK: u32 = 160;
        const RB_VFPORCH: u32 = 3;
        let budget = (HV_FACTOR * 1_000_000).checked_sub(RB_MIN_VBLANK * HV_FACTOR * field)?;
        hperiod = budget / (u64::from(vdisplay) * field);
        if hperiod == 0 {
            return None;
        }
        let lines = u32::try_from(RB_MIN_VBLANK * HV_FACTOR / hperiod + 1).ok()?;
        let vbilines = lines.max(RB_VFPORCH + vsync + MIN_V_BPORCH);
        vtotal = vdisplay + vbilines;
        htotal = hdisplay + RB_H_BLANK;
        hsync_end = hdisplay + RB_H_BLANK / 2;
        hsync_start = hsync_end - RB_H_SYNC;
        vsync_start = vdisplay + RB_VFPORCH;
        hsync_high = true;
    } else {
        const MIN_VSYNC_BP: u64 = 550;
        const HSYNC_PERCENTAGE: u32 = 8;
        // CVT's M' and C': M * K / 256 and (C - J) * K / 256 + J.
        const M_PRIME: u64 = 600 * 128 / 256;
        const C_PRIME: u64 = (40 - 20) * 128 / 256 + 20;
        let budget = (HV_FACTOR * 1_000_000).checked_sub(MIN_VSYNC_BP * HV_FACTOR * field)?;
        let lines = (u64::from(vdisplay) + u64::from(MIN_V_PORCH)) * 2;
        hperiod = budget * 2 / (lines * field);
        if hperiod == 0 {
            return None;
        }
        let back = u32::try_from(MIN_VSYNC_BP * HV_FACTOR / hperiod + 1).ok()?;
        let sync_and_back = back.max(vsync + MIN_V_PORCH);
        vtotal = vdisplay + sync_and_back + MIN_V_PORCH;
        let percentage = (C_PRIME * HV_FACTOR)
            .saturating_sub(M_PRIME * hperiod / 1000)
            .max(20 * HV_FACTOR);
        let blank = u64::from(hdisplay) * percentage / (100 * HV_FACTOR - percentage);
        let blank = u32::try_from(blank - blank % u64::from(2 * H_GRANULARITY)).ok()?;
        htotal = hdisplay + blank;
        hsync_end = hdisplay + blank / 2;
        let start = hsync_end.checked_sub(htotal * HSYNC_PERCENTAGE / 100)?;
        hsync_start = start + H_GRANULARITY - start % H_GRANULARITY;
        vsync_start = vdisplay + MIN_V_PORCH;
        hsync_high = false;
    }
    let clock = u64::from(htotal) * HV_FACTOR * 1000 / hperiod;
    let span = |value: u32| u16::try_from(value).ok();
    let timing = Timing {
        clock_khz: u32::try_from(clock).ok()?,
        hdisplay: span(hdisplay)?,
        hsync_start: span(hsync_start)?,
        hsync_end: span(hsync_end)?,
        htotal: span(htotal)?,
        vdisplay: span(vdisplay)?,
        vsync_start: span(vsync_start)?,
        vsync_end: span(vsync_start + vsync)?,
        vtotal: span(vtotal)?,
        hsync_high,
        vsync_high: !hsync_high,
    };
    timing.is_mode().then_some(timing)
}

/// Whether a timing has CVT's reduced blanking, as Linux's
/// `drm_mode_is_rb` judges it.
#[must_use]
pub fn is_reduced(timing: &Timing) -> bool {
    timing.htotal.wrapping_sub(timing.hdisplay) == 160
        && timing.hsync_end.wrapping_sub(timing.hdisplay) == 80
        && timing.hsync_end.wrapping_sub(timing.hsync_start) == 32
        && timing.vsync_start.wrapping_sub(timing.vdisplay) == 3
}

/// The timing a standard timing or an Established Timings III bit names:
/// VESA's own (DMT) where it has one, as Linux's `drm_mode_find_dmt`
/// finds it, or else CVT's.
#[must_use]
pub fn standard(width: u32, height: u32, hertz: u32, reduced: bool) -> Option<Timing> {
    DMT.iter()
        .find(|timing| {
            (u32::from(timing.hdisplay), u32::from(timing.vdisplay)) == (width, height)
                && timing.refresh_hz() == hertz
                && is_reduced(timing) == reduced
        })
        .copied()
        .or_else(|| cvt(width, height, hertz, reduced))
}

/// Bytes of an EDID block.
const BLOCK: usize = 128;
/// Where a base block's detailed timing descriptors are.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];

/// Every mode `bytes`, an EDID, names, into `list`; the first detailed
/// timing, or a `DisplayID` timing marked preferred, preferred. Bytes that
/// are not an EDID's -- too short, a header, checksum or version that is
/// not one -- add nothing, and every read is bounded by `bytes`: an EDID is
/// a device's word, read and never trusted.
pub fn edid_modes(bytes: &[u8], list: &mut ModeList) {
    let Some(base) = bytes.get(..BLOCK) else {
        return;
    };
    let header = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    let sums = |block: &[u8]| block.iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte)) == 0;
    if base.get(..8) != Some(&header[..]) || !sums(base) || base.get(18) != Some(&1) {
        return;
    }
    // The detailed timings first, the first of them preferred (EDID 1.4,
    // and 1.3's feature bit 1, which every monitor sets).
    let mut first = true;
    for &at in &DESCRIPTORS {
        if let Some(timing) = base.get(at..at + 18).and_then(detailed) {
            let _ = list.add(timing, first);
            first = false;
        }
    }
    // The extensions' detailed timings and video codes come before the
    // base block's names for modes, as they are the display's own.
    let extensions = usize::from(base.get(126).copied().unwrap_or(0));
    for index in 1..=extensions {
        let Some(block) = bytes.get(index * BLOCK..(index + 1) * BLOCK) else {
            break;
        };
        if !sums(block) {
            continue;
        }
        match block.first() {
            Some(0x02) => cta_modes(block, list),
            Some(0x70) => displayid_modes(block, list),
            _ => {}
        }
    }
    // Established timings: bytes 35 and 36, and byte 37's top bit.
    let word = u32::from(base.get(35).copied().unwrap_or(0))
        | u32::from(base.get(36).copied().unwrap_or(0)) << 8
        | u32::from(base.get(37).copied().unwrap_or(0) & 0x80) << 9;
    for (bit, timing) in ESTABLISHED.iter().enumerate() {
        if word & (1 << bit) != 0
            && let Some(timing) = timing
        {
            let _ = list.add(*timing, false);
        }
    }
    // Standard timings: eight pairs at 38.
    for pair in base.get(38..54).unwrap_or(&[]).chunks_exact(2) {
        if let Some(timing) = standard_timing(pair) {
            let _ = list.add(timing, false);
        }
    }
    // Established Timings III: a display descriptor of tag 0xF7, its bits
    // in bytes 6 to 11, each byte's from bit 7 down.
    for &at in &DESCRIPTORS {
        let Some(block) = base.get(at..at + 18) else {
            continue;
        };
        if block.get(..3) != Some(&[0, 0, 0][..]) || block.get(3) != Some(&0xf7) {
            continue;
        }
        for (byte_index, &byte) in block.get(6..12).unwrap_or(&[]).iter().enumerate() {
            for bit in 0..8 {
                if byte & (0x80 >> bit) == 0 {
                    continue;
                }
                let Some(&(wide, tall, hertz, reduced)) = ESTABLISHED3.get(byte_index * 8 + bit)
                else {
                    continue;
                };
                if let Some(timing) =
                    standard(u32::from(wide), u32::from(tall), u32::from(hertz), reduced)
                {
                    let _ = list.add(timing, false);
                }
            }
        }
    }
}

/// An 18-byte detailed timing descriptor, as Linux's `drm_mode_detailed`
/// reads it: `None` for a display descriptor (clock 0), an interlaced or
/// stereo mode's, or one that is not a mode.
fn detailed(block: &[u8]) -> Option<Timing> {
    let byte = |at: usize| block.get(at).copied().map(u32::from);
    let clock = byte(0)? | byte(1)? << 8;
    if clock == 0 || byte(17)? & 0x80 != 0 {
        return None;
    }
    let hactive = byte(2)? | (byte(4)? >> 4) << 8;
    let hblank = byte(3)? | (byte(4)? & 0xf) << 8;
    let vactive = byte(5)? | (byte(7)? >> 4) << 8;
    let vblank = byte(6)? | (byte(7)? & 0xf) << 8;
    let hsync_offset = byte(8)? | (byte(11)? >> 6) << 8;
    let hsync_pulse = byte(9)? | ((byte(11)? >> 4) & 0x3) << 8;
    let vsync_offset = (byte(10)? >> 4) | ((byte(11)? >> 2) & 0x3) << 4;
    let vsync_pulse = (byte(10)? & 0xf) | (byte(11)? & 0x3) << 4;
    build(
        clock * 10,
        [hactive, hblank, hsync_offset, hsync_pulse],
        [vactive, vblank, vsync_offset, vsync_pulse],
        byte(17)? & 0x2 != 0,
        byte(17)? & 0x4 != 0,
    )
}

/// A timing from active, blanking, sync offset and sync width each way,
/// with Linux's repairs: a sync that runs past the blanking makes the
/// total one past it.
fn build(
    clock_khz: u32,
    [hactive, hblank, hoffset, hpulse]: [u32; 4],
    [vactive, vblank, voffset, vpulse]: [u32; 4],
    hsync_high: bool,
    vsync_high: bool,
) -> Option<Timing> {
    let span = |value: u32| u16::try_from(value).ok();
    let hsync_start = hactive + hoffset;
    let hsync_end = hsync_start + hpulse;
    let vsync_start = vactive + voffset;
    let vsync_end = vsync_start + vpulse;
    let htotal = (hactive + hblank).max(hsync_end + 1);
    let vtotal = (vactive + vblank).max(vsync_end + 1);
    let timing = Timing {
        clock_khz,
        hdisplay: span(hactive)?,
        hsync_start: span(hsync_start)?,
        hsync_end: span(hsync_end)?,
        htotal: span(htotal)?,
        vdisplay: span(vactive)?,
        vsync_start: span(vsync_start)?,
        vsync_end: span(vsync_end)?,
        vtotal: span(vtotal)?,
        hsync_high,
        vsync_high,
    };
    timing.is_mode().then_some(timing)
}

/// A standard timing's two bytes, as EDID 1.3 and 1.4 have them: `None`
/// for an unused pair (1, 1) or a size no table or CVT times. Linux's
/// 1366x768 repair (1360x765 named) is kept.
fn standard_timing(pair: &[u8]) -> Option<Timing> {
    let (&first, &second) = (pair.first()?, pair.get(1)?);
    if (first, second) == (1, 1) || first == 0 {
        return None;
    }
    let width = (u32::from(first) + 31) * 8;
    let height = match second >> 6 {
        0 => width * 10 / 16,
        1 => width * 3 / 4,
        2 => width * 4 / 5,
        _ => width * 9 / 16,
    };
    let hertz = u32::from(second & 0x3f) + 60;
    if (width, height) == (1360, 765) {
        return standard(1366, 768, hertz, false).or_else(|| standard(1360, 768, hertz, false));
    }
    standard(width, height, hertz, false)
}

/// A CTA-861 extension's video codes and detailed timings.
fn cta_modes(block: &[u8], list: &mut ModeList) {
    let Some(&dtd_at) = block.get(2) else {
        return;
    };
    let dtd_at = usize::from(dtd_at);
    // The data block collection, from byte 4 to where the descriptors
    // start; each block a header (tag in bits 7:5, length in 4:0) and that
    // many bytes.
    let mut at = 4;
    while at < dtd_at.min(BLOCK - 1) {
        let Some(&head) = block.get(at) else {
            break;
        };
        let len = usize::from(head & 0x1f);
        if head >> 5 == 2 {
            for &svd in block.get(at + 1..(at + 1 + len).min(dtd_at)).unwrap_or(&[]) {
                // Linux's `svd_to_vic`: bit 7 marks a native code below 65.
                let vic = if (1..=64).contains(&svd) || (129..=192).contains(&svd) {
                    svd & 0x7f
                } else {
                    svd
                };
                if let Some((_, timing)) = CTA.iter().find(|(code, _)| *code == vic) {
                    let _ = list.add(*timing, false);
                }
            }
        }
        at += 1 + len;
    }
    if dtd_at >= 4 {
        let mut at = dtd_at;
        while at + 18 < BLOCK {
            match block.get(at..at + 18).and_then(detailed) {
                Some(timing) => {
                    let _ = list.add(timing, false);
                }
                None => break,
            }
            at += 18;
        }
    }
}

/// A `DisplayID` extension's type I detailed timings, as Linux's
/// `drm_mode_displayid_detailed` reads them: each field one less than it
/// is, the sync polarities in the offsets' top bits, and the options'
/// bit 7 the preferred one.
fn displayid_modes(block: &[u8], list: &mut ModeList) {
    let Some(&len) = block.get(2) else {
        return;
    };
    let end = 5 + usize::from(len);
    if end > BLOCK - 1 {
        return;
    }
    let mut at = 5;
    while at + 3 <= end {
        let (Some(&tag), Some(&size)) = (block.get(at), block.get(at + 2)) else {
            return;
        };
        let size = usize::from(size);
        if at + 3 + size > end {
            return;
        }
        if tag == 0x03 {
            for timing in block
                .get(at + 3..at + 3 + size)
                .unwrap_or(&[])
                .chunks_exact(20)
            {
                if let Some((timing, preferred)) = displayid_timing(timing) {
                    let preferred = preferred && !list.has_preferred();
                    let _ = list.add(timing, preferred);
                }
            }
        }
        at += 3 + size;
    }
}

/// One `DisplayID` type I timing's 20 bytes, and whether it is marked
/// preferred: `None` for an interlaced one, or one that is not a mode.
fn displayid_timing(timing: &[u8]) -> Option<(Timing, bool)> {
    let byte = |at: usize| timing.get(at).copied().map(u32::from);
    let word = |at: usize| Some(byte(at)? | byte(at + 1)? << 8);
    let options = byte(3)?;
    if options & 0x10 != 0 {
        return None;
    }
    let clock = (byte(0)? | byte(1)? << 8 | byte(2)? << 16) + 1;
    let made = build(
        clock.checked_mul(10)?,
        [
            word(4)? + 1,
            word(6)? + 1,
            (word(8)? & 0x7fff) + 1,
            word(10)? + 1,
        ],
        [
            word(12)? + 1,
            word(14)? + 1,
            (word(16)? & 0x7fff) + 1,
            word(18)? + 1,
        ],
        word(8)? & 0x8000 != 0,
        word(16)? & 0x8000 != 0,
    )?;
    Some((made, options & 0x80 != 0))
}
