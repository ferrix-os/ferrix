//! Copies on pipe A's vertical blank: when the frame a FLUSH carries is
//! written into the lit plane, so that it shows whole and on a refresh of
//! its own.
//!
//! The card copies each frame into the memory the firmware's plane scans
//! out of (`super`). Done the moment a FLUSH came, the copy landed wherever
//! the beam was: the rows above it showed the new frame one refresh later
//! than the rows below it (a tear), and two `FLUSH`es inside one refresh both
//! counted as shown though the screen showed only the second. A KMS driver
//! answers a page flip at the vertical blank it latched in
//! (`drm_crtc_send_vblank_event` from i915's flip-done interrupt), and a
//! compositor paces itself by that event. This is the same for a copy:
//! a FLUSH of the shown buffer waits for pipe A's next vertical blank, the
//! copy is made then -- 2.4 MB at 1024x600 takes about a millisecond, far
//! ahead of the beam that starts down the screen after the blank -- and
//! FLIPPED is answered after it, so the flip's completion is the refresh it
//! is shown in.
//!
//! The wait is the serving loop's own, never a sleep: the render node is
//! served by the same loop (`crate::serve`), and a game's GPU work must not
//! wait for the screen. The FLUSH is kept ([`Pending`]) and the loop's
//! wake-up set for just before the blank; the last part of the wait, at
//! most [`SPIN_NS`], reads the scanline until the blank begins, since a
//! wake a little late would miss a blank of under a millisecond and cost a
//! whole refresh.
//!
//! Where the beam is comes from `PIPEDSL`, the blank's place from the
//! transcoder's `TRANS_VTOTAL`, both read and never written. A refresh's
//! length is measured on `PIPE_FRMCOUNT`. A transcoder whose numbers do not
//! make sense leaves the card copying at once, as before.

use ferrix_intel_xelp::io::Mmio;

use super::regs::Regs;

/// `_PIPEADSL`: the scanline pipe A is on, in bits 12:0.
const PIPEDSL_A: u32 = 0x7_0000;
/// `_PIPEA_FRMCOUNT_G4X`: frames pipe A has started.
const PIPE_FRMCOUNT_A: u32 = 0x7_0040;
/// `_TRANS_VTOTAL_A`: (vtotal - 1) << 16 | (vactive - 1).
const TRANS_VTOTAL_A: u32 = 0x6_000C;
/// A line number's bits.
const LINE: u32 = 0x1fff;

/// Lines past the top of the active area a copy may still start on: it
/// writes a line in under 2 µs and the beam takes 26, so it stays ahead.
const EARLY_LINES: u32 = 4;
/// The longest the loop reads the scanline waiting for the blank; a wake
/// sooner than that before it is set instead.
pub(crate) const SPIN_NS: u64 = 1_500_000;
/// Frames a refresh is measured over.
const MEASURE_FRAMES: u32 = 120;

/// Pipe A's beam.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Beam {
    vactive: u32,
    vtotal: u32,
    /// A refresh, in nanoseconds: the listed rate's until measured.
    frame_ns: u64,
    /// `PIPE_FRMCOUNT` and the time it was read, to measure from.
    since: (u32, u64),
}

impl Beam {
    /// Pipe A's beam, or `None` when its transcoder's timing does not make
    /// sense for the plane (`height` rows) or the scanline is not moving.
    pub(crate) fn read(regs: &mut Regs, height: u32, refresh_hz: u64, now: u64) -> Option<Self> {
        let vt = regs.read32(TRANS_VTOTAL_A);
        let vactive = (vt & LINE) + 1;
        let vtotal = ((vt >> 16) & LINE) + 1;
        if vactive < 100 || vtotal <= vactive || vtotal > vactive * 2 || vactive < height {
            return None;
        }
        let first = scanline(regs);
        let moving = (0..10_000).any(|_| scanline(regs) != first);
        if !moving || refresh_hz == 0 {
            return None;
        }
        Some(Self {
            vactive,
            vtotal,
            frame_ns: 1_000_000_000 / refresh_hz,
            since: (regs.read32(PIPE_FRMCOUNT_A), now),
        })
    }

    /// The active lines, the total, and a refresh in nanoseconds.
    pub(crate) const fn timing(&self) -> (u32, u32, u64) {
        (self.vactive, self.vtotal, self.frame_ns)
    }

    /// Nanoseconds until the next vertical blank begins; 0 while the beam
    /// is in one, or only just past it.
    pub(crate) fn until_blank(&mut self, regs: &mut Regs, now: u64) -> u64 {
        self.measure(regs, now);
        let line = scanline(regs);
        if line >= self.vactive || line < EARLY_LINES {
            return 0;
        }
        u64::from(self.vactive - line) * self.frame_ns / u64::from(self.vtotal)
    }

    /// The refresh, measured again over the last [`MEASURE_FRAMES`].
    fn measure(&mut self, regs: &mut Regs, now: u64) {
        let count = regs.read32(PIPE_FRMCOUNT_A);
        let frames = count.wrapping_sub(self.since.0);
        if frames >= MEASURE_FRAMES {
            let ns = now.saturating_sub(self.since.1) / u64::from(frames);
            // A refresh between 20 and 250 Hz; anything else is a pipe
            // that stopped or was retimed under the measure.
            if (4_000_000..=50_000_000).contains(&ns) {
                self.frame_ns = ns;
            }
            self.since = (count, now);
        }
    }
}

/// The scanline pipe A is on.
fn scanline(regs: &mut Regs) -> u32 {
    regs.read32(PIPEDSL_A) & LINE
}

/// A FLUSH waiting for the blank.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pending {
    pub(crate) buffer: u32,
    pub(crate) sequence: u64,
    pub(crate) rect: ferrix_displayctl::message::Rect,
    /// When it came.
    pub(crate) at: u64,
}

/// What the card's flips did, said every [`REPORT_NS`].
#[derive(Debug, Default)]
pub(crate) struct Tally {
    flips: u32,
    waited: u32,
    wait_ns: u64,
    longest_wait_ns: u64,
    /// The scanline was in the active area when the copy started: the wake
    /// was too late, or the spin ran out.
    torn: u32,
    last: Option<u64>,
    intervals: u32,
    over_50: u32,
    over_66: u32,
    longest: u64,
    since: u64,
    copies: u32,
    copy_ns: u64,
    longest_copy_ns: u64,
    evict_ns: u64,
    /// Frames the blitter copied, and from submit to seen done.
    blits: u32,
    blit_ns: u64,
    longest_blit_ns: u64,
}

/// How often the tally is said.
pub(crate) const REPORT_NS: u64 = 10_000_000_000;

impl Tally {
    /// A frame copied at `now`, `waited` after its FLUSH came, `torn`
    /// when it did not start in the blank.
    pub(crate) fn flip(&mut self, now: u64, waited: u64, torn: bool) {
        self.flips += 1;
        if waited > 0 {
            self.waited += 1;
            self.wait_ns += waited;
            self.longest_wait_ns = self.longest_wait_ns.max(waited);
        }
        self.torn += u32::from(torn);
        if let Some(last) = self.last {
            let gap = now.saturating_sub(last);
            self.intervals += 1;
            self.over_50 += u32::from(gap > 50_000_000);
            self.over_66 += u32::from(gap > 66_000_000);
            self.longest = self.longest.max(gap);
        }
        self.last = Some(now);
    }

    /// A copy into a plane that took `ns`, `evicted` of it evicting its
    /// source from the caches.
    pub(crate) fn copied(&mut self, ns: u64, evicted: u64) {
        self.copies += 1;
        self.evict_ns += evicted;
        self.copy_ns += ns;
        self.longest_copy_ns = self.longest_copy_ns.max(ns);
    }

    /// A frame the blitter copied, seen done `ns` after its submit.
    pub(crate) fn blitted(&mut self, ns: u64) {
        self.blits += 1;
        self.blit_ns += ns;
        self.longest_blit_ns = self.longest_blit_ns.max(ns);
    }

    /// The tally's line once [`REPORT_NS`] went by, and a new tally.
    pub(crate) fn report(&mut self, now: u64) -> Option<Self> {
        if self.since == 0 {
            self.since = now;
            return None;
        }
        if now.saturating_sub(self.since) < REPORT_NS || self.flips == 0 {
            return None;
        }
        let done = core::mem::take(self);
        self.since = now;
        self.last = done.last;
        Some(done)
    }

    pub(crate) fn say(&self) {
        ferrix_rt::info!(
            "intel: card0: flips {} in 10 s, {} waited for the blank (mean {} us, longest {} us), {} not in it; gaps over 50 ms {}, over 66 ms {}, longest {} ms; copies {} (mean {} us, {} us of it evicting, longest {} us); blits {} (submit to seen done mean {} us, longest {} us)",
            self.flips,
            self.waited,
            self.wait_ns / u64::from(self.waited.max(1)) / 1_000,
            self.longest_wait_ns / 1_000,
            self.torn,
            self.over_50,
            self.over_66,
            self.longest / 1_000_000,
            self.copies,
            self.copy_ns / u64::from(self.copies.max(1)) / 1_000,
            self.evict_ns / u64::from(self.copies.max(1)) / 1_000,
            self.longest_copy_ns / 1_000,
            self.blits,
            self.blit_ns / u64::from(self.blits.max(1)) / 1_000,
            self.longest_blit_ns / 1_000,
        );
    }
}
